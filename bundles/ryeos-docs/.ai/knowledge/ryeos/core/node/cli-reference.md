<!-- ryeos:signed:2026-11-14T03:56:24Z:b4c4a3a7130ab706770b861959cf1727a990da2e84316e6cdb8f39a3d13b57ca:Wav3X9Go2rZfzI0PLJh63NeGRieF8nD1bFl0h/mohme6rTdb9CQdZJLE28HuRtq+uaT3+rof8uukDvSLRF5EDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
   `node status`, `node doctor`, the `node reset ...` family, the `trust ...` verbs,
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
profile's declared host classes (`full` declares `local` and
`local_unconfined`). `local` enforces the sandbox lane: init refuses when the
host's sandbox probe cannot prove it, and the refusal names
`--host-class local_unconfined`, the explicit opt-out that runs jobs as the
daemon user. Init never selects the opt-out for you, and `ryeos node doctor`
warns while a node runs it.
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

A running node on a native-capacity issuance also reports its **service
window**: what remains of this incarnation before its planned rollover. The
rollover is an orderly stop that begins at whichever comes first: the signed
`rollover_drain_margin_ms` ahead of the service edge (the edge itself is
`maximum_occupancy_ms − cleanup_occupancy_ms` after the window opened, on
`CLOCK_BOOTTIME`), or the signed `rollover_work_margin_steps` before the
prepaid service work runs out. The JSON report carries it as
`Running.service_window`:

| Field | Meaning |
|---|---|
| `seconds_until_rollover` | seconds until the drain margin is reached |
| `work_steps_until_rollover` | native work (SQL VM steps) left before the work margin |
| `rolls_at` | wall-clock (UTC) estimate of `seconds_until_rollover`; the daemon schedules on `CLOCK_BOOTTIME`, not on this |

The text report shows one row, for example `service window: 73 min / 61000000
work steps until rollover (about 2026-11-13T02:05:00Z)`. A script that is about
to run a chain of placements compares these numbers with the sum of the chain's
declared needs (each placement's `timeout_seconds + cleanup_timeout_seconds`
and `native_work_steps`), and waits for the rollover when they do not fit. Do
not derive the window from `daemon.json` `started_at`: that is wall-clock time
and ignores both margins. The node admits each fresh external placement against
the same numbers and refuses one that does not fit with
`native_headroom_insufficient`, carrying `dimension` (`service_seconds` or
`work_steps`), `declared` and `remaining`. To open a fresh window on purpose,
`ryeos stop && ryeos start`; that costs one incarnation of the epoch, while an
orderly rollover renews it.

### `ryeos node doctor`

```bash
ryeos node doctor [--json] [--no-bundles] [--app-root <dir>]
```

Runs the offline node checklist. Its isolation row uses the production strict
loader: disabled is a warning (runs are unconfined; re-init or cut a policy
with an enforce lane), while enforced mode resolves
and inspects the selected signed backend bundle and validates resource limits.
Policy edits require a daemon restart; see
[Execution Isolation](execution-isolation.md). Its `process_quarantine` row
warns about threads whose attached process group recovery cannot prove gone,
from the report the daemon writes after each recovery pass. Its
`stale_publication` row lists `publish_ready` hosted candidates whose base HEAD
has moved (session, candidate, base and current HEAD); each settles to
`base_superseded` on its owner's next publish.

The `service_window` row reports the running node's service window (see
`ryeos node status`) as read for this doctor run: `ok` with
`seconds_until_rollover`, `work_steps_until_rollover` and `rolls_at` while the
node runs, and `n/a` while it is stopped or reports no window. It judges
nothing; the caller compares it with what it is about to run.

The `key_age` row lists the node, operator, authoring and publisher keys with
when each entered service and its age in days. It reports age only and warns
when the node has no active operator or authoring key (`ryeos init` creates
both). Node key rotation is not offered.

The `authorized_keys` row reads every node-signed grant without starting the
node. It fails for a grant of a retired schema or class (`grant_schema_stale`,
`grant_class_stale`, which also stop the daemon from starting) or one that
cannot be read, and warns for a grant that has expired or will within the
policy's `warn_before_secs`. Each entry names the grant and its cure: renew a
lapsed grant with `ryeos renew-key --fingerprint <fp>` (the principal that
granted it, or the node operator); re-issue a stale one.

The `remote_pins` row verifies every node-signed remote pin record under
`.ai/node/remotes/` and reports each remote's pin source. It fails, naming the
cure, for a record that is unsigned (`remote_pin_unsigned`), signed by a key
other than the node key (`remote_pin_wrong_signer`), edited after signing
(`remote_pin_tampered`) or otherwise unusable (`remote_pin_invalid`), and for
an unsigned remotes config file this node no longer reads
(`remote_config_file_unsupported`): re-pin each remote with
`ryeos remote configure <name> --url <url> --expect-fingerprint <fingerprint>`,
then delete the file.

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
allocations, open Q local starts and `retained_generations` (runtime snapshot
binding generations a recompose superseded while a qualification occurrence or
bootstrap source made under them is still open, each with the operations that
reference it), `credential_profiles` every held profile lock with the session,
Q start or continuation reservation that holds it.
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

### `ryeos node external-execution snapshot-compose`

```bash
ryeos node external-execution snapshot-compose --id <binding-id> \
  (--binding <external-execution-id> | --backend <backend> --account <label> \
   --owner-id <owner> --roots <roots.pem>) \
  --provider-spec <snapshot-spec.json> --sandbox-group-id <sbg-...> \
  --region <region> --source-plan <starter|standard|pro> \
  [--contact-timeout-seconds <s>] [--maximum-bootstrap-lifetime-seconds <s>] \
  [--maximum-upload-bytes <n>] [--app-root <dir>] [--json]
```

Composes one node-signed `runtime_snapshot_production` binding on a stopped
node. Backend, account, owner and provider API trust roots come either from an
installed external-execution binding (`--binding`) or are stated together; the
adapter artifact comes from the installed bundles, and `--provider-spec` must
be the snapshot production spec the installed adapter declares. Every run names
a fresh credential generation: provision it with `provision-credential
--section runtime_snapshot_production`, then restart the node.

### `ryeos node external-execution snapshot-qualification-compose`

```bash
ryeos node external-execution snapshot-qualification-compose --id <binding-id> \
  --snapshot-binding <production-id> --provider-spec <provider-spec.json> \
  --verifier <restoration-verifier-hash> [--consumer-verifiers <file>] \
  [--contact-timeout-seconds <s>] [--delivery-timeout-seconds <s>] \
  [--maximum-lifetime-seconds <s>] [--app-root <dir>] [--json]
```

Composes one `runtime_snapshot_qualification` binding anchored to an installed
production binding: it pins that binding's id and digest and reuses its
provider settings. `--consumer-verifiers` is a JSON object mapping scenario id
to a consumer verifier selection (`scenario_source_digest`,
`verifier_artifact_hash`, `archive_budget`), the output of the running node's
consumer qualification context derive. It uses the producer's credential.

### `ryeos node external-execution qualification-session-compose`

```bash
ryeos node external-execution qualification-session-compose --id <binding-id> \
  (--binding <external-execution-id> | --backend <backend> --account <label> \
   --owner-id <owner> --roots <roots.pem>) \
  --policy-ref <ref> --provider-declaration-id <id> \
  --runtime-manifest-hash <hash> [--guest-runtime-manifest-hash <hash>] \
  --runtime-member-sha256 <sha> --snapshot-id <snp-...> --plan <plan> \
  --region <region> --private-mailbox-parent <dir> [--capacity-group <group>] \
  [--limits <limits.json>] [--app-root <dir>] [--json]
```

Composes the qualification-only structured-session placement binding a
consumer qualification's Worker session is placed under, restored from the
provider snapshot `--snapshot-id`. Artifact identities come from the installed
bundles; every run names a fresh credential generation to provision.

### `ryeos node external-execution recompose`

```bash
ryeos node external-execution recompose [--binding <section>/<id>]... \
  [--consumer-verifiers <file>] [--dry-run] [--app-root <dir>] [--json]
```

Re-pins installed bindings to the installed bundles after a payload change, on
a stopped node. Without `--binding` it recomposes every binding whose pins the
`external_bindings` doctor row fails; a named binding is recomposed even when
coherent (to take new consumer selections, for example). The order is fixed:
`runtime_snapshot_production` bindings, then every
`runtime_snapshot_qualification` binding that names a recomposed producer (it
follows the producer's new digest), then `external_execution` bindings. Only
artifact identities change: adapter, supervisor and launcher generations,
restoration verifier, provider and snapshot specs, connector and configuration
adapter, and the producer digest a qualification pins. Every fact the operator
stated is carried, and so is the credential generation: the vault keys a
credential by signer, backend, account and generation, none of which a re-pin
changes, so no secret is provisioned again. An explicit compose still names a
fresh generation.

A qualification that selects consumer verifiers needs `--consumer-verifiers`
(the derive output); its scenario set is carried and each selection is taken
from the file. Refusals are typed and publish nothing:

| Code | Meaning |
|---|---|
| `recompose_artifact_not_installed` | nothing installed fills a pinned role (adapter, restoration verifier, consumer verifier, provider executables) |
| `recompose_settings_schema_changed` | the installed adapter declares another settings schema; restate the settings with `compose` |
| `recompose_consumer_verifiers_required` | a qualification to recompose selects consumer verifiers and no file was given |
| `recompose_consumer_selection_missing` | the file has no selection for a scenario the qualification selects |
| `recompose_binding_not_installed` | a named binding, or the producer a qualification names, is not installed |

A rerun converges: a re-pin that changes nothing publishes nothing. A
`binding_credential_absent` finding is listed, not recomposed. `--dry-run`
prints what would be recomposed. Hosted controllers accept
`node.external-execution.recompose` from their origin operator as a signed
offline operation, as they do `compose`. Recompose never waits on open work:
a qualification occurrence or bootstrap source keeps the generation it was
made under and is terminated through it; see
[Runtime Snapshots](runtime-snapshots.md).

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

### `ryeos external-content runtime-snapshot-run`

```bash
ryeos external-content runtime-snapshot-run '{"production_binding":"render-snapshot","qualification_binding":"render-snapshot-qualification","source":{"kind":"captured_product","witness_hash":"<witness>","witness_source":{"kind":"local_capture"}},"maximum_lifetime_seconds":600}'
```

Produces and qualifies one runtime snapshot on a running node through the
node's own services, in order: `bootstrap-runtime-source` (the node creates the
source sandbox, for a captured product or a bundle materialization),
`observe-bootstrap-runtime-source` until it runs, `produce-runtime-snapshot`
joined to that bootstrap operation, `observe-runtime-snapshot-readiness`,
`terminate-bootstrap-runtime-source`, `create-runtime-snapshot-qualification`,
`observe-runtime-snapshot-qualification` until the restored occurrence runs,
`verify-runtime-snapshot-qualification`, and
`terminate-runtime-snapshot-qualification` unless `keep_occurrence` is true
(the report then gives the occurrence's deadline from the qualification
binding's `maximum_lifetime_seconds` and the terminate verb).

Before any provider contact it refuses if `ryeos node doctor`'s binding check
fails either named binding. It keeps no state file: every step's operation id
derives from its coordinates, so rerunning the same input reads the node's
journals and continues. A step the node has not settled (an attempt pending,
quarantined, late or rejected) stops the run with `stopped` naming the step,
its phase and the read verb for its record; the run never repeats provider
contact. The provider credential is only the production binding's, in the node
vault.

`verify-runtime-snapshot-qualification` observes the restored occurrence first
and refuses with HTTP 409 `occurrence_not_running` (status `creating`,
`terminated`, `absent`, ...) before claiming its one verifier attempt, so a
verify against a sandbox the provider is still creating no longer spends it.
`produce-runtime-snapshot` still accepts a captured product with an
operator-named `source_occurrence_id` instead of a `bootstrap_operation_id`
until the external snapshot driver is retired.

## Other local operator verbs

- `ryeos trust ...` — manage the node's signer registry (table below).
- `ryeos authorize-key --public-key <ed25519:...> --label <label> --scopes <scope,...>` — authorize a caller locally.
- `ryeos remote-descriptor --name <name> --url <url>` — ask the running daemon for this node's signed remote descriptor (a trust pin signed by the node key inside the daemon). The reply carries the descriptor and its signed `yaml`.
- `ryeos admission-token --label <label> --scopes <scope,...> [--ttl-secs <seconds>]` — mint a one-time local admission token file for remote bootstrap.
- `ryeos publish <bundle-dir> --key <private-key.pem> --owner <label>` — sign/publish bundle contents.

## Trust verbs

The signer registry lives in `<app_root>/.ai/node/trust/`. See
[Signing and Trust](../signing.md) for roles, scopes and statuses.

| Verb | Effect |
|---|---|
| `ryeos trust pin --from <PUBLISHER_TRUST.toml> --role publisher\|project_author\|authoring [--project <root>]... [--space <space>] [--kind <kind>]` | Pin a signer with the given role and scope |
| `ryeos trust scope <fp> [--space <space>] [--kind <kind>] [--project <root>]...` | Replace a signer's scope |
| `ryeos trust retire <fp>` | Retire a signer; it still verifies existing items |
| `ryeos trust revoke <fp>` | Revoke a signer; it verifies nothing and cannot be pinned again |
| `ryeos trust succeed --from <succession.toml>` | Apply a signed succession statement |
| `ryeos trust list` | List signer records (read-only) |
| `ryeos trust show <fp>` | Show one signer record and its key events (read-only) |

`list` and `show` work on a running or stopped node. The five mutating
verbs are signed offline operations (`node.trust.pin`, `node.trust.scope`,
`node.trust.retire`, `node.trust.revoke`, `node.trust.succeed`; local signer
only). On a running node they refuse with `node_running_trust_change`; cure
by stopping the node, or by `ryeos node offline-operation submit --stop`.
Each change appends a node-signed, hash-chained key event under
`<app_root>/.ai/node/trust/events/`.

### Key rotation

| Verb | Effect |
|---|---|
| `ryeos node operator rotate [--json]` | Stage a new operator key, record the succession from the old key, re-issue the operator grant, replace the genesis and reseal the init completion; the old key is retired |
| `ryeos node authoring rotate [--json]` | Stage a new authoring key and record the succession; the new key keeps the old key's scope |

Both are signed offline operations (`node.operator.rotate`,
`node.authoring.rotate`; local signer only; the operator one is applied alone).
They refuse on a running node with `node_running_trust_change`. See
[Identity Model](../identity-model.md#key-lifecycle) for what each does and the
refusal codes.

## Core daemon-backed verbs

- `ryeos external-execution status` — list unsettled external allocations, open
  Q local starts, superseded snapshot binding generations open work still
  references, and held credential-profile locks, each with its owner,
  attention and fix. Local configured operator only; the doctor's
  `external_capacity` and `credential_profiles` rows read it.
- `ryeos execute <ref> [params...]` — execute an item by canonical ref.
- `ryeos content pin <ref> --id <declaration>` — atomically complete a signed
  external-content pin through the daemon-owned, local-operator authoring path.
- `ryeos external-content bindings <consumer-ref> <installed_bundle|pinned_project> [project-snapshot-hash]`
  — list a consumer's content bindings and whether each still authorizes it at
  that generation; read only, local operator.
- `ryeos external-content product publish <producer-ref> <product-name> [--activation <config-ref>] [--from-thread <thread-id>] [--push-to <remote>]`
  — produce, capture, admit and optionally push one product, as one client
  composite over the verified commands; see
  [Retained products](../execution/retained-products.md).
- `ryeos admission submit <subject-hash> [--policy <id>] [--claim accepted]` —
  admit a local CAS root under the node's local admission policy (idempotent
  per subject and policy).
- `ryeos external-content push-product <remote> <witness-hash> <admission-hash> [maximum-bytes]`
  — push an admitted product; the bound defaults to the declaration's.
- `ryeos external-content rebind <prior-binding-hash> <project-snapshot-hash>` —
  renew a pinned-project content binding for a new snapshot or re-signed
  consumer; local operator, refused with a typed code when the consumer changed.
- `ryeos fetch <ref> [--with-content] [--verify]` — resolve/read an item. Alias: `f`.
- `ryeos sign <ref> [--source project|user]` — sign an item. Alias: `s`.
- `ryeos external-execution inventory <section>/<binding-id>` — list, read
  only, the sandboxes and snapshots the provider holds for one installed
  `external_execution` or `runtime_snapshot_production` binding's owner scope,
  through its adapter and vault credential. Each row is joined with the node's
  allocation, bootstrap-source, qualification-occurrence and snapshot journals
  of the same backend and classed `known_live`, `leaked` (the node's record
  ended, the provider still runs it), `unknown` (no node record: another node,
  a hand-made resource, or a quarantined allocation's lost answer) or `gone`
  (the node holds it open, the provider no longer runs it; only from a
  complete listing, and a record of another account of the same backend can
  show here). Nothing is deleted. The result is a node-signed CAS receipt;
  the reply carries its digest, the rows and the counts. Listing is bounded
  (256 rows); a cut listing is `complete: false`.
- `ryeos external-execution settle-unknown-allocation <placement> <reason> <inventory-receipt>`
  — release one quarantined allocation that never bound an occurrence, citing
  an inventory receipt by digest. Refused with `settle_inventory_refused`
  (409) unless the receipt is this node's signed, complete listing of the
  allocation's provider backend and account, taken after the allocation's
  last possible contact, with no `unknown` running sandbox created inside the
  allocation's contact window (two minutes of clock skew either side). The
  reason stays free text; the receipt digest is recorded with it. A receipt is
  an unreferenced CAS object, so `ryeos maintenance gc` may collect it; take a
  fresh inventory then.
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

- `ryeos remote pin <name> --descriptor <file>` — pin a remote from its
  signed v2 descriptor without contacting it, as a node-signed pin record; a
  descriptor without a URL pins an inbound-only remote (outbound commands
  refuse it with `remote_inbound_only`).
- `ryeos node offline-operation sign --operation node.authorize-client|node.remote.pin|node.bundle.replace ...`
  — wire a hosted node that delegates these operations: a remote grant, a
  remote pin (`--input descriptor=<file>`) or a bundle installed from an
  archive (`--input archive=<file.tar.gz>`). See "Wiring a hosted node
  without a shell" in [Remote Operations](../remote/remote-operations.md).
- `ryeos node offline-operation facts|submit|list --remote <name>` — read a
  remote's facts, deliver a request signed with your own key (with its input
  files), or read what the remote queued and applied, through your own node
  under the delegation you stored for that remote. The delegation must lend
  `ryeos.execute.service.offline-operations/facts`, `/submit` and `/status`
  (`ryeos remote delegate --remote <name> --scopes ...`); an inbound-only
  remote is refused with `remote_inbound_only`.
- `ryeos node offline-operation sign --facts <file>` signs under the authority
  the facts name for the signing key (`signer`): the local operator signs
  without a grant, and a remote operator's request takes its grant digest and
  origin site from the facts. `facts --json` reports the signer for
  `--signing-key` (default the operator key), `facts --remote` for you as the
  delegated principal. Facts without a signer, read for another key, or for a
  key the node holds no usable grant for are refused with
  `offline_operation_facts_without_signer`,
  `offline_operation_facts_for_another_signer` and
  `offline_operation_signer_ungranted`.
See [Remote Command Reference](../remote/remote-command-reference.md).

## Aliases Quick Reference

| Alias | Verb |
|---|---|
| `f` | `fetch` |
| `s` | `sign` |
