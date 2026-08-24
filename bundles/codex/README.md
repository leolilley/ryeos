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
creation mask and descriptor-safely strips group/other mode bits at worker IPC
boundaries; this is generic private-home enforcement, not knowledge of Codex
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
backend. The official Codex package's own private `codex-resources/bwrap` is a
separately pinned workload resource used by Codex to enforce the narrower
model-command permission profile; it is not a RyeOS launch backend. There is no
custom credential bridge, token injection, local-LLM route, worker pool, or
cross-session process reuse.

The exact Codex executable, same-version code-mode host, and the package's
`bwrap`, `zsh`, and `rg` runtime resources are assembled from OpenAI's pinned
standalone package with `assemble.py`. Import and bind all five files through
the ordinary `external-content import`/`external-content bind` ceremony. Their
expected manifest hashes and individual file checksums are fixed in
the activation declaration at `.ai/config/codex/activation.yaml`, which is the
machine-readable source of the import bounds and checksums.

Persistent subprocesses are deliberately disabled when the node has no
operator-owned policy. Before starting the daemon, install a state-space
`.ai/node/persistent_sessions/policy.yaml` with `schema: 1` and the exact
`persistent_session_policy` limits from the activation declaration (nested as
`limits:`). The shipped baseline admits four dedicated workers; lower those
values for a smaller node. A bundle never silently enables this node-wide
capacity.

## Operator flow

This workflow is intended for a dedicated hosted endpoint. Complete Codex
external-content import/bind and node policy setup while the hosted configured
operator is still a `local_client`. Provision the same configured-operator key
at the source and hosted nodes. Separately admit the source node key on the
hosted target as `remote_node` with only the generic forwarding-attestation
scope:

```sh
FORWARDING_SCOPE='ryeos.attest.request.forwarded-operator'
# Hosted target, while its daemon is still locally maintainable:
ryeos admission-token --label 'hosted operator forwarding node' \
  --scopes "$FORWARDING_SCOPE" --ttl-secs 600
# Source node, using the one-time token printed above:
ryeos remote admit --remote hosted --token '<one-time-token>' \
  --label 'hosted operator forwarding node' --scopes "$FORWARDING_SCOPE"
```

Then stop the hosted daemon and explicitly replace the configured-operator
key's hosted `local_client` grant with a target-node-signed `remote_operator`
grant. This is a semantic class conversion, not a scope update:

```sh
HOSTED_SCOPES='ryeos.execute.worker_execution.codex/login,ryeos.execute.worker_execution.codex/session,ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.system/push-head,ryeos.execute.service.credential-profiles/create,ryeos.execute.service.credential-profiles/get,ryeos.execute.service.credential-profiles/revoke,ryeos.execute.service.credential-profiles/confirm,ryeos.execute.service.credential-profiles/delete,ryeos.execute.service.worker-executions/status,ryeos.execute.service.worker-executions/command,ryeos.execute.service.worker-executions/approvals,ryeos.execute.service.worker-executions/resolve-approval,ryeos.execute.service.worker-executions/terminate,ryeos.execute.service.worker-executions/publish,ryeos.execute.service.worker-executions/validate-candidate-closure-and-base,ryeos.execute.service.worker-executions/discard,ryeos.write.project.live'
ryeos-core-tools authorize-client \
  --app-root /path/to/hosted-app-root \
  --public-key "<configured_operator_raw_ed25519_base64>" \
  --label "hosted operator forwarded from source" \
  --origin-site-id "site:<source>" \
  --allow-semantic-conversion \
  --scopes "$HOSTED_SCOPES"
```

Use the exact `site_id` from the source node's identity. The operator grant is
an allowed-source constraint, not source proof by itself. Each forwarded
request is signed first by the configured operator and then co-signed over the
exact request authorization by the separately admitted source-node key. The
target accepts `remote_operator` only when both grants, the co-signature, the
site, and the caller-signed required-origin assertion agree. A key holder
calling the hosted daemon directly has no source-node proof and is rejected.

There is one authorized-key file per fingerprint. Local-only external-content
maintenance therefore uses the same configured-operator key in an explicit
quiesced class-transition ceremony: finish or terminate hosted sessions, stop
the hosted daemon, and run:

```sh
MAINTENANCE_SCOPES='ryeos.execute.service.external-content/import,ryeos.execute.service.external-content/bind,ryeos.execute.service.external-content/release,ryeos.execute.service.external-content/scrub'
ryeos-core-tools authorize-client \
  --app-root /path/to/hosted-app-root \
  --public-key "<configured_operator_raw_ed25519_base64>" \
  --label "hosted operator local maintenance" \
  --allow-semantic-conversion \
  --scopes "$MAINTENANCE_SCOPES"
```

The tool mechanically acquires the daemon's exclusive state lock before a
semantic conversion and refuses while the daemon owns it. Start the daemon and
perform maintenance, stop it again, then reinstall the exact `remote_operator`
grant above with `--allow-semantic-conversion`. A separate key cannot satisfy
the configured-operator check. Never merge scopes across a class or origin
transition.

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
    "session_id": "LOGIN_SESSION",
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
# This opt-in generic push signs the destination HEAD as the configured
# operator. Ordinary `ryeos remote push` remains node-owned.
ryeos execute service:remote/push --input - <<'JSON'
{
  "remote": "hosted",
  "project": "/local/project",
  "outbound_principal": "configured_operator"
}
JSON
REMOTE_LAUNCH_ID="L-$(uuidgen | tr -d '-')"
ryeos execute service:remote/run --input - <<JSON
{
  "remote": "hosted",
  "item_ref": "worker_execution:codex/session",
  "ref_bindings": {},
  "project": "/local/project",
  "outbound_principal": "configured_operator",
  "parameters": {"credential_profile_id": "personal"},
  "launch_id": "$REMOTE_LAUNCH_ID",
  "execution_policy": {
    "schema_version": 2,
    "ownership": "daemon_owned",
    "recovery": "restart_recoverable",
    "response": "accepted",
    "target": {"kind": "here"},
    "environment": {
      "kind": "project_overlay",
      "include_operator_vault": true,
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
  }
}
JSON
```

The same configured-operator key must exist at both operator endpoints and use
the origin-bound hosted grant above. The operator-owned push and launch
deliberately use that key; ordinary node-key remote authorization cannot create
or control this operator-owned workflow. A plain `local_client` grant is also
incorrect: it erases forwarding origin and is rejected by this activation
contract.

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

Approval decisions require the exact pending request digest. Permission,
network, exec-policy, session-wide, legacy patch, and legacy exec expansions
are deny-only; an accepted operation retries under the unchanged
`ryeos-workspace-only`
profile. Termination is explicit and publication is a separate terminal CAS.
Use the same projectless remote service envelope with the signed generic
services `worker-executions/resolve-approval`, `terminate`,
`validate-candidate-closure-and-base`, `publish`, or `discard`; their exact
parameters and digest fences are declared by their signed service items.

The remote operator grant must carry only the exact execution and service
scopes for the Codex worker-execution item, credential-profile endpoints,
worker-execution endpoints, object upload, project HEAD publication, and live
project publication. External-content services are local-only and must not be
present. The separate source-node `remote_node` grant needs only
`ryeos.attest.request.forwarded-operator`. The hosted-node policy rejects
wildcard grants; the feature does not broaden a client's authority.

Profile homes are plaintext node-private state visible to the node operator.
They are capped at 2 GiB and survive sessions until explicit revoke/delete.
Sessions have a seven-day hard lifetime and one active worker per profile.
Pushed batches, individual facts, bridge queues, and command/result ledgers are
bounded; retained observations use the ordinary root thread-chain retention
contract rather than a second Codex journal. RyeOS records live OpenAI inference using Codex-managed ChatGPT
authentication; reported plan type is an observation, not proof of a
subscription tier.
