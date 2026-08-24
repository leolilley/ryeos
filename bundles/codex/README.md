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

This workflow is intended for a dedicated hosted endpoint. Provision the same
configured-operator key at the source and hosted nodes, then—with the hosted
daemon stopped—replace that key's hosted authorized-key entry with a
target-node-signed `remote_operator` grant. The grant preserves operator
ownership and independently binds the authenticated source site; it never
turns a remote call into a local one:

```sh
HOSTED_SCOPES='ryeos.execute.worker.codex/login,ryeos.execute.worker.codex/session,ryeos.execute.service.objects/has,ryeos.execute.service.objects/put,ryeos.execute.service.system/push-head,ryeos.execute.service.credential-profiles/create,ryeos.execute.service.credential-profiles/get,ryeos.execute.service.credential-profiles/revoke,ryeos.execute.service.credential-profiles/confirm,ryeos.execute.service.credential-profiles/delete,ryeos.execute.service.worker-executions/status,ryeos.execute.service.worker-executions/command,ryeos.execute.service.worker-executions/approvals,ryeos.execute.service.worker-executions/resolve-approval,ryeos.execute.service.worker-executions/terminate,ryeos.execute.service.worker-executions/publish,ryeos.execute.service.worker-executions/validate-candidate-closure-and-base,ryeos.execute.service.worker-executions/discard,ryeos.write.project.live,ryeos.execute.service.external-content/import,ryeos.execute.service.external-content/bind,ryeos.execute.service.external-content/scrub,ryeos.execute.service.external-content/release'
ryeos-core-tools authorize-client \
  --app-root /path/to/hosted-app-root \
  --public-key "<configured_operator_raw_ed25519_base64>" \
  --label "hosted operator forwarded from source" \
  --origin-site-id "site:<source>" \
  --scopes "$HOSTED_SCOPES"
```

Use the exact `site_id` from the source node's identity. The origin is carried
by the hosted node's signed grant, not by caller data. The forwarded HEAD and
execute bodies include a signed required-origin assertion solely so a missing
or mismatched target grant fails closed; the assertion cannot create origin.
Because one authorized-key file exists per fingerprint, all use of this key on
the hosted target is classified as remote-origin; use a separate key for
target-local maintenance.

The bundle registers terminal aliases over the same authenticated daemon
services used by any remote RyeOS client. A minimal session is:

```sh
ryeos codex profile create personal

# Enrollment is projectless. Use the returned thread_id as LOGIN_SESSION. The
# device response is ephemeral: display its URL/code and do not journal it.
ryeos codex login open personal --async
ryeos codex session command LOGIN_SESSION login-1 credential.login.start \
  --payload '{}'

# After completing the browser/device ceremony, read only sanitized account
# metadata, then close the short-lived login worker while retaining its profile.
ryeos codex session command LOGIN_SESSION account-1 credential.account.read \
  --payload '{}'
ryeos codex session terminate LOGIN_SESSION completed
ryeos codex profile get personal
ryeos codex profile confirm personal LOGIN_EPOCH EXPECTED_ACCOUNT_DIGEST

# Project sessions require the now-active profile and an existing
# principal-scoped project HEAD. On the hosted node use `ryeos commit`.
ryeos --project . commit "Codex hosted-session base"
ryeos --project . codex session start personal --async --current-head

# Use the returned thread_id as SESSION.
ryeos codex session command SESSION thread-1 session.start \
  --payload '{}'
ryeos codex session command SESSION turn-1 turn.start \
  --payload '{"input":[{"type":"text","text":"Implement the requested change","text_elements":[]}]}'

# App Server notifications are pushed into the root RyeOS thread event chain
# before the worker receives its acknowledgement. Reattach or replay through
# the ordinary thread/SSE surfaces; there is no second Codex event journal.
ryeos codex session approvals SESSION
```

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
RyeOS root/session ID. Drive its
projectless session endpoints with the same configured-operator key directly
against the hosted daemon (for example by setting `RYEOSD_URL` for the CLI),
or from a RyeOS UI connected to that daemon. These requests remain attributed
to the configured source site even when sent directly because the target grant
is the authenticated origin boundary. No shared filesystem pathname is
required after admission.

Approval decisions require the exact pending request digest. Permission,
network, exec-policy, session-wide, legacy patch, and legacy exec expansions
are deny-only; an accepted operation retries under the unchanged
`ryeos-workspace-only`
profile. Termination is explicit and publication is a separate terminal CAS:

```sh
ryeos codex session approval SESSION APPROVAL_ID REQUEST_DIGEST false
ryeos codex session terminate SESSION completed
ryeos codex session validate candidate SESSION CANDIDATE_HASH CANDIDATE_VALIDATION_HASH
ryeos codex session publish SESSION EXPECTED_BASE_HASH
# Or, instead of publication:
ryeos codex session discard SESSION CANDIDATE_HASH
```

The remote authorized key must carry the exact execution and service scopes
for the Codex worker-execution item, credential-profile endpoints,
worker-execution endpoints, and external-content import/bind. The hosted-node policy rejects
wildcard grants; the feature does not broaden a client's authority.

Profile homes are plaintext node-private state visible to the node operator.
They are capped at 2 GiB and survive sessions until explicit revoke/delete.
Sessions have a seven-day hard lifetime and one active worker per profile.
Pushed batches, individual facts, bridge queues, and command/result ledgers are
bounded; retained observations use the ordinary root thread-chain retention
contract rather than a second Codex journal. RyeOS records live OpenAI inference using Codex-managed ChatGPT
authentication; reported plan type is an observation, not proof of a
subscription tier.
