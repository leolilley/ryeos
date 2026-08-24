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
The activation declaration at `.ai/config/codex/activation.yaml` is the
machine-readable source of the import bounds and checksums.

Persistent subprocesses are deliberately disabled when the node has no
operator-owned policy. Before starting the daemon, install a state-space
`.ai/node/persistent_sessions/policy.yaml` with `schema: 1` and the exact
`persistent_session_policy` limits from the activation declaration (nested as
`limits:`). The shipped baseline admits four dedicated workers; lower those
values for a smaller node. A bundle never silently enables this node-wide
capacity.

## Operator flow

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
# principal-scoped project HEAD. On the hosted node use `ryeos commit`; from a
# client node use the standard full-project `ryeos remote push` workflow.
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
