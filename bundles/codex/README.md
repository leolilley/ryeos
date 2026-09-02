<!-- ryeos:signed:2026-09-02T12:38:43Z:12e586e238bed42fdbd19224dd52e743b1a0b83d43f84548ba58357baaa91ce1:jRqI7MQuAoTnjO/eIzownqJ89/FnMCsyYFPGUnwvprBKOm64nqnRKLd8ejHHW3XgDjpJjpqzWq5/VbBV2XqbDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# RyeOS Codex

First-class signed integration for hosting the pinned Codex App Server on a
RyeOS node. Codex-specific structured-session semantics live in signed bundle
data and the bundle-owned bridge; durable process, session, workspace,
authorization, and publication authority remain in generic RyeOS substrate.

This bundle does not provide RyeOS local inference and does not place ChatGPT
credentials in NodeVault or bundle content.

Installed knowledge separates the generic substrate from this integration:
`knowledge:ryeos/core/execution/worker-hosted-execution` defines worker-hosted
execution, while `knowledge:codex/hosted-activation` defines the Codex
activation and release-acceptance runbook shipped by this bundle.

## Runtime contract

The integration admits OpenAI Codex `0.147.0` as external large content and
runs its App Server behind the signed `ryeos-structured-session-bridge` using
the admission-compiled profile embedded in `worker:codex/hosted`. This is the common closed
structured-session boundary, not a Codex kind or a Codex branch in the engine.
Authentication is Codex's supported ChatGPT device-code flow. Codex durably manages `auth.json`
inside one daemon-owned, mode-0700 opaque profile `CODEX_HOME`; RyeOS never
parses or copies the token document. The common bridge applies an owner-only
creation mask and reasserts owner-only access on the exact pinned home root at
worker IPC boundaries; stopped homes receive the complete bounded
descriptor-relative validation before attachment. The exact mode-0700 pinned
root is the privacy boundary; descendant mode bits remain opaque workload
state. This is generic private-home enforcement, not knowledge of Codex
filenames. The pinned `ryeos-workspace-only`
permission profile denies the filesystem root, reopens only Codex's minimal
runtime paths, and keeps the private CoW project writable while command
networking stays disabled. Security-critical settings are repeated as immutable
signed process arguments and are the sole configuration authority, including
the built-in OpenAI provider and an empty MCP-server map. RyeOS atomically
resets the mode-0400 compatibility config before every worker generation; the
workload may rewrite that seed, but it is neither retained policy nor a
same-UID integrity boundary. When a generic RyeOS isolation backend is enabled
it also overlays that file read-only, but Codex activation does not require
RyeOS's optional Bubblewrap isolation bundle or any other RyeOS isolation
backend. OpenAI's pinned standalone package does require its own packaged
`codex-resources/bwrap` companion for restricted Linux command execution. The
activation imports that exact workload-owned file beside Codex; it does not
select RyeOS's isolation backend, install or discover a host `bwrap`, or
provide BusyBox.
There is no custom credential bridge, token injection, local-LLM route, worker
pool, or cross-session process reuse.

The exact Codex executable, same-version code-mode host, packaged command-
sandbox companion, and the package's `zsh` and `rg` runtime resources are
selected from OpenAI's pinned standalone package by two signed acquisition
recipes. `config:codex/activation` supplies the worker's five file
realizations. `config:codex/environment-activation`
supplies the default environment's self-contained `bin/{zsh,rg}` developer-
tool tree from the same verified archive. The generic `external-content
activate` service downloads or reuses the exact archive, verifies its archive
and member digests under node policy, imports through the existing manifest
authority, publishes ordinary consumer bindings, and records one compact
node-signed completion receipt per consumer. There is no public assembly
directory, installed assembler, manually authored manifest, or second
realization authority.

Persistent subprocesses and managed acquisition are deliberately disabled
when their node-owned policies are absent. Before starting the daemon, apply
an external-content policy with no named roots and an explicit managed
activation host/resource ceiling, plus the exact persistent-session limits.
Then, while the configured operator is still local to that node, run both
`ryeos external-content activate config:codex/activation online` and `ryeos
external-content activate config:codex/environment-activation online`. The
complete typed YAML and ceremony live in `knowledge:codex/hosted-activation`.
A bundle never silently enables network acquisition, storage, or node-wide
worker capacity.

## Operator flow

This workflow is intended for a dedicated hosted endpoint. Complete Codex
managed activation and node policy setup with the hosted node's own local
operator. Keep that target-local signing key on the target. The source
operator's private key remains only at the source; the target receives its
public key later in a node-signed `remote_operator` grant. Separately admit the
source node key on the hosted target as `remote_node` with only the generic
forwarding-attestation scope:

```sh
FORWARDING_SCOPE='ryeos.attest.request.forwarded-operator'
# Hosted target, while its daemon is still locally maintainable:
ryeos admission-token --label 'hosted operator forwarding node' \
  --scopes "$FORWARDING_SCOPE" --ttl-secs 600
# Source node, using the one-time token printed above:
ryeos remote admit --remote hosted --token '<one-time-token>' \
  --label 'hosted operator forwarding node' --scopes "$FORWARDING_SCOPE"
```

Then stop the hosted daemon and install the source operator's public key in a
target-node-signed `remote_operator` grant. A fresh hosted node has no incumbent
grant for that fingerprint and needs no semantic conversion. If an exact
incumbent grant for that source key is being reclassified or rebound, use
`--allow-semantic-conversion` explicitly and never merge scopes across the
transition:

```sh
HOSTED_SCOPES='ryeos.execute.config.codex/environments/default,ryeos.execute.worker_execution.codex/login,ryeos.execute.worker_execution.codex/session,ryeos.execute.service.events/chain_replay,ryeos.execute.service.launch/status,ryeos.execute.service.launch/cancel,ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.system/push-head,ryeos.execute.service.threads/tail,ryeos.execute.service.credential-profiles/create,ryeos.execute.service.credential-profiles/get,ryeos.execute.service.credential-profiles/revoke,ryeos.execute.service.credential-profiles/confirm,ryeos.execute.service.credential-profiles/delete,ryeos.execute.service.worker-executions/status,ryeos.execute.service.worker-executions/command,ryeos.execute.service.worker-executions/approvals,ryeos.execute.service.worker-executions/resolve-approval,ryeos.execute.service.worker-executions/terminate,ryeos.execute.service.worker-executions/checkpoint,ryeos.execute.service.worker-executions/resume,ryeos.execute.service.worker-executions/handoff-preflight,ryeos.execute.service.worker-executions/handoff,ryeos.execute.service.worker-executions/publish,ryeos.execute.service.worker-executions/validate-candidate-closure-and-base,ryeos.execute.service.worker-executions/discard,ryeos.write.project.live'
RYEOS_APP_ROOT=/path/to/hosted-app-root ryeos authorize-client \
  --public-key "<configured_operator_raw_ed25519_base64>" \
  --label "hosted operator forwarded from source" \
  --origin-site-id "site:<source>" \
  --scopes "$HOSTED_SCOPES"
```

Use the exact `site_id` from the source node's identity. The operator grant is
an allowed-source constraint, not source proof by itself. Each forwarded
request is signed first by the configured operator and then co-signed over the
exact request authorization by the separately admitted source-node key. The
target accepts `remote_operator` only when both grants, the co-signature, the
site, and the caller-signed required-origin assertion agree. A key holder
calling the hosted daemon directly has no source-node proof and is rejected.

Those are owner-facing Codex/session scopes. Portable placement does not add
its internal services to the operator grant. Instead, each placement peer's
node key receives the provider-neutral exact node ceiling documented in
`knowledge:ryeos/core/execution/worker-hosted-execution`: bounded closure read,
worker-placement preflight/prepare/adopt/abort, and follow-terminal delivery.
The source and target daemons use those node-authenticated contracts only
after the owner has authorized `handoff`; the peer node never becomes the
session owner. This separation permits handoff back to a node whose operator
key remains an ordinary local client.

The hosted node's own local operator and the forwarded source operator are
different principals with different authorized-key files. Local-only
external-content maintenance therefore uses the target-local operator without
reclassifying the forwarded source grant: finish or terminate hosted sessions,
then run only the exact maintenance scopes through the target's local CLI.
When narrowing that target-local bootstrap grant from `*`, replace its scope
set explicitly through the same verified node-signed grant writer:

```sh
MAINTENANCE_SCOPES='ryeos.execute.service.external-content/activate,ryeos.execute.service.external-content/release,ryeos.execute.service.external-content/scrub'
RYEOS_APP_ROOT=/path/to/hosted-app-root ryeos authorize-client \
  --public-key "<target_local_operator_raw_ed25519_base64>" \
  --label "hosted target local maintenance" \
  --scopes "$MAINTENANCE_SCOPES"
```

Same-class grant creation and scope replacement use descriptor-pinned
compare-and-swap publication and daemon hot reload. A real class or origin
transition additionally requires `--allow-semantic-conversion`, mechanically
acquires stopped-node authority, and refuses while the daemon owns it. Perform
maintenance through the target-local operator; the independently stored source
`remote_operator` grant is unchanged throughout. Never merge scopes across a
class or origin transition.

The bundle registers daemon-local terminal aliases, but an activated hosted
target deliberately rejects direct operator-key HTTP requests because they
lack the source-node co-signature. Drive it from the source RyeOS node through
the provider-neutral `service:remote/run` seam. For example, create the empty
credential profile with a projectless, wait-mode service execution:

```sh
ryeos execute service:remote/run --input - <<'JSON'
{
  "remote": "hosted",
  "item_ref": "service:credential-profiles/create",
  "ref_bindings": {},
  "outbound_principal": "configured_operator",
  "parameters": {"profile_id": "personal"},
  "execution_policy": {
    "schema_version": 2,
    "ownership": "daemon_owned",
    "recovery": "restart_recoverable",
    "response": "wait",
    "target": {"kind": "here"},
    "environment": {"kind": "none"},
    "project": {"kind": "projectless"}
  }
}
JSON
```

Open the projectless login worker through the same seam, this time retaining a
launch coordinate and using `accepted` response semantics:

```sh
LOGIN_LAUNCH_ID="L-$(uuidgen | tr -d '-')"
ryeos execute service:remote/run --input - <<JSON
{
  "remote": "hosted",
  "item_ref": "worker_execution:codex/login",
  "ref_bindings": {},
  "outbound_principal": "configured_operator",
  "parameters": {"credential_profile_id": "personal"},
  "launch_id": "$LOGIN_LAUNCH_ID",
  "execution_policy": {
    "schema_version": 2,
    "ownership": "daemon_owned",
    "recovery": "restart_recoverable",
    "response": "accepted",
    "target": {"kind": "here"},
    "environment": {"kind": "none"},
    "project": {"kind": "projectless"}
  }
}
JSON
```

Retain the returned remote `thread_id` as `LOGIN_SESSION`. Send the first
structured command with a projectless wait-mode `service:remote/run` envelope
like the first example, changing only `item_ref` and `parameters`:

```json
{
  "item_ref": "service:worker-executions/command",
  "parameters": {
    "chain_root_id": "LOGIN_SESSION",
    "idempotency_key": "login-1",
    "route_id": "credential.login.start",
    "payload": {}
  }
}
```

The device URL/code in the result is ephemeral: display it and do not journal
it. After the browser ceremony, use the same command service with route
`credential.account.read`; terminate the login execution through
`service:worker-executions/terminate`; read and confirm the profile through
`service:credential-profiles/get` and `service:credential-profiles/confirm`.
The exact parameter schemas are the signed service items. App Server
notifications are pushed into the root RyeOS thread chain before worker
acknowledgement; reattach/replay uses the ordinary thread/event surfaces, not
a second Codex journal.

When the client and hosted node have different absolute project paths, first
configure a standard full-project remote binding and create the remote HEAD
under the configured operator. Then launch through the provider-neutral
`service:remote/run` seam so the destination path comes from that binding
rather than from the client path:

```sh
# The destination path must already be a valid, canonicalizable project root
# on the hosted node. The binding keeps host-local paths out of execution
# authority and selects full-project snapshot transport.
ryeos remote bind-project hosted \
  --project /local/project \
  --remote-project /hosted/project \
  --sync-scope full_project

# This opt-in push signs the destination HEAD as the configured operator.
# Omitting outbound-principal preserves ordinary node-owned push semantics.
ryeos remote push hosted \
  --project /local/project \
  --outbound-principal configured_operator

REMOTE_LAUNCH_ID="L-$(uuidgen | tr -d '-')"
ryeos remote run hosted worker_execution:codex/session \
  --project /local/project \
  --outbound-principal configured_operator \
  --parameters '{"credential_profile_id":"personal"}' \
  --ref-bindings '{"environment":"config:codex/environments/default"}' \
  --launch-id "$REMOTE_LAUNCH_ID" \
  --execution-policy '{
    "schema_version": 2,
    "ownership": "daemon_owned",
    "recovery": "restart_recoverable",
    "response": "accepted",
    "target": {"kind": "here"},
    "environment": {
      "kind": "project_overlay",
      "include_operator_vault": false,
      "name_policy": {"kind": "declared_required"}
    },
    "project": {
      "kind": "pinned",
      "source": {"kind": "current_head"},
      "realization": {
        "kind": "cow",
        "terminal_publication": {"kind": "retain_current_head"}
      },
      "child_policy": {"kind": "inherit"}
    }
  }'
```

The signed Codex environment declares only its exact non-secret developer-tool
tree. Authentication comes from the selected credential profile's private
home, so this launch must keep `include_operator_vault: false`; the worker has
no operator-vault dependency or access.

If a later handoff preflight reports that the peer's owner project HEAD is not
the placement base, stop before handoff and explicitly converge the two valid
project histories. Supply both observed hashes and choose which generation's
tree/policy wins; RyeOS publishes a two-parent generation remote-first and
then locally under a durable recovery job:

```sh
ryeos remote reconcile-project-head hosted \
  --project /local/project \
  --expected-local-head '<local-head>' \
  --expected-remote-head '<hosted-head>' \
  --winner remote
```

This is ordinary provider-neutral project synchronization. It neither adds a
handoff override nor broadens the hosted operator grant: the target-side calls
reuse its existing object upload/HEAD scopes, and the peer node's bounded
closure-read authority supplies the exact remote generation.

The source operator key exists only at the source endpoint. The hosted target
retains only its public key in the origin-bound grant above. Operator-owned
push and launch deliberately preserve that source principal; ordinary node-key
remote authorization cannot create or control the workflow. Classifying the
source key as a plain `local_client` at the target is incorrect because it
erases forwarding origin and is rejected by this activation contract.

The coordinate is printed before remote contact; retain it until the accepted
response echoes the same value. The returned `result.thread_id` is the remote
RyeOS root/session ID. Drive its projectless status, command, approval,
termination, validation, publication, and discard services through
projectless wait-mode `service:remote/run` calls from the source node, always
selecting `outbound_principal: configured_operator`. This generic service path
is also the backend contract for a future RyeOS UI, but neither the current UI
browser-session principal nor this bundle supplies that configured-operator
forwarding workflow. Direct target requests are intentionally rejected because
operator-key possession alone does not prove source-node transit. No shared
filesystem pathname is required after admission.

Approval decisions require the exact pending request digest. All approval
classes are deny-only in this release, including command execution: Codex's
App Server command request can represent a sandbox escalation without exposing
a complete reviewable permission delta. RyeOS may display the bounded request
and send decline/cancel, but cannot accept it. The immutable `on-request`
policy lets supported approval requests reach the RyeOS ledger; the profile's
`deny_only` contract
rejects an accept decision before upstream contact. Termination is explicit
and publication is a separate terminal CAS.
Use the same projectless remote service envelope with the signed generic
services `worker-executions/resolve-approval`, `terminate`,
`validate-candidate-closure-and-base`, `publish`, or `discard`; their exact
parameters and digest fences are declared by their signed service items.

`terminate` accepts exactly `completed` or `cancelled`. A completed project
session freezes its candidate and can then be checkpointed; cancellation is a
direct terminal disposition and cannot be checkpointed. `resume` consumes the
exact frozen `manifest_ref` and creates a fresh placement under the stable
chain root.

The remote operator grant must carry only the exact execution and service
scopes for the Codex worker-execution item, its declared environment config,
credential-profile endpoints, worker-execution endpoints, chain-event replay
and tail attach, object upload, project HEAD publication, and live project
publication.
External-content services are local-only and must not be present. The separate
source-node `remote_node` grant needs only
`ryeos.attest.request.forwarded-operator`. The hosted-node policy rejects
wildcard grants; the feature does not broaden a client's authority.

Profile homes are plaintext node-private state visible to the node operator.
They are capped at 2 GiB and survive sessions until explicit revoke/delete.
Sessions have a seven-day hard lifetime and one active worker per profile.
Pushed batches, individual facts, bridge queues, and command/result ledgers are
bounded. A session accepts at most 1,048,576 worker events across all worker
epochs; SQLite keeps only one cumulative settled predecessor frontier and any
ambiguous outbox body. Complete retained observations use the ordinary root
thread-chain retention contract rather than a second Codex journal. RyeOS records live OpenAI inference using Codex-managed ChatGPT
authentication; reported plan type is an observation, not proof of a
subscription tier.
