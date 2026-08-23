<!-- ryeos:signed:2026-08-23T23:14:20Z:65fb2c59de51cd888b12a0a671cdd73b140ad3f09685270cd4eb73a9e6362061:YQtEBQLdIFboKyZOyldnmLFguG7gwIuHqeT3+K9llzeDMhWiX36s261ZSF3aSX89IDCvHC7HulCOeTT0yOQeBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/core/execution"
name: "worker-hosted-execution"
title: "Worker-Hosted Execution"
description: "Implemented authority, protocol, lifecycle, recovery, and publication contracts for session-bound hosted workers"
entry_type: reference
version: "1.0.0"
```

# Worker-Hosted Execution

Worker-hosted execution runs one long-lived subordinate workload for one
ordinary RyeOS root execution. It is generic execution substrate. Codex is its
first structured-session consumer; it is not an engine kind, agent identity,
or local-inference implementation.

This is installed RyeOS runtime knowledge shipped by the standard bundle. It
describes the authority visible to operators and authored integrations; it is
not a repository implementation plan.

## Authority and ownership

The root thread, root event chain, and `AdmittedLaunchCapsule` remain the only
execution authority. The launch capsule binds pinned project authority and the
exact subordinate `AdmittedPersistentSessionCapsule`. The subordinate capsule
binds executable, lifecycle, wire protocol, complete canonical
structured-session contract, local schema hashes, source closure, route/effect
ceilings, workspace authority, and resource limits.

Worker, session, command, approval, and observation rows are operational
projections. They cannot mint execution, project, credential, or publication
authority. Lillux's attached process identity is process-control authority.
Recovery reconciles it with both admitted capsules; it never reconstructs
authority from a mutable worker row or registry resolution.

Every profile and worker-execution entry point admits only the exact configured
operator fingerprint. Authenticated requests may use local or remote transport;
local transport is not the predicate. Owner rows are defense in depth and do
not claim hostile multi-principal isolation.

## Closed structured-session protocol

`ryeos.structured-session` is a fixed Rust protocol family, not a plugin or
general transformation language. Signed worker data can only select and narrow
its admitted vocabulary. Admission parses the full profile, rejects unknown
fields, compiles every local JSON schema, rejects remote references, validates
bounded routes/templates/predicates/observations/server requests, and embeds
the complete canonical contract plus schema hashes in the subordinate capsule.
The bridge executes only the exact captured profile digest.

Public commands contain an admitted route ID and schema-validated payload.
Direction, audience, effect class, fixed/workspace parameters, forbidden
authority fields, response predicates, retention, ceremony effects, and remote
session binding are capsule-bound. Clients never submit upstream methods or
RyeOS control frames. Runtime resume/read routes are not public. Unsupported
semantics require a reviewed Rust capability; authored data cannot become code.

The inherited target socket is full duplex. A persistent reader demultiplexes
responses and pushed observation batches while independent bounded queues keep
control moving. Batches bind session, worker, boot epoch, sequence, and digest.
RyeOS appends canonical facts to the root chain before acknowledgement. Stale,
duplicate, uncorrelated, unknown, or over-budget output cannot advance authority.

## Lifecycle and evidence

Process lifecycle is separate from session projection, command contact, the
orthogonal approval set, and workspace disposition. A completed project session
closes and freezes its CoW candidate, then waits for explicit validation and
publish or discard while the root stays running. A projectless enrollment
session has no candidate and becomes terminal directly.

Command contact is durable-before-write. A committed command in a dead epoch
without contact becomes a stable retryable-uncontacted failure; contacted or
ambiguous work is never replayed. Root facts retain canonical command,
idempotency identity, route/request digest, boot epoch, subordinate capsule,
profile, and schema identities. A successful response first appends one
canonical redacted command-observation batch to the root chain; only then are
events, approvals, session observations, and the result projection advanced.
Restart can therefore rebuild a dispatched command from that batch instead of
incorrectly downgrading authoritative success to outcome-unknown.

Approval consent covers one exact action inside the admitted ceiling. It never
expands authority. The outbox reserves the decision, records possible contact
before socket write, and distinguishes settled from delivery-unknown. Startup
idempotently completes missing decision/contact/unknown root facts without
refiring possible contact. Listing approvals is read-only.

Worker facts are `worker_asserted` or `upstream_reported`, not proof of success.
Daemon contact is `daemon_observed_io`; candidate checks are
`filesystem_verified`; publication is `owner_authorized`. Subscription plan
metadata is upstream testimony, not entitlement proof.

## Credentials and recovery

RyeOS owns an opaque mode-0700 profile home and generation/operation lock.
Pinned Codex owns its supported file credential format and refresh; RyeOS never
parses or journals tokens. One active session per profile serializes login,
refresh, logout, revoke, and restart. Credentials are plaintext node-private
state visible to the configured operator.

Login is projectless and generation-bound. Device material uses only the
confidential ephemeral response lane. Owner confirmation of sanitized account
identity precedes project use. Cancellation, expiry, disconnect, and restart
invalidate the ceremony and allow a fresh login epoch.

Revocation enters durable `revoking` before reaping workers or removing the
home. Admission, attachment, readiness, command, approval, and recovery recheck
the exact generation and lock. Unproved death retains worker identity and the
credential fence; the home is removed only after every worker proves cleanup.
Cleanup and lock release are transactional or resumable at durable boundaries.
One generic per-profile operation coordinator covers start through readiness,
every worker contact, termination, confirmation, revocation, and deletion.
Root ownership is always acquired before profile ownership.

In-memory retirement returns distinct `reaped`, `unproved`, `reserved`, and
`absent` evidence. Reservation or registry absence is never process-death
proof. If attachment fails after spawn, the exact Lillux process identity is
persisted as unproved before control returns; failure to persist that evidence
keeps the credential lock fenced.

## Workspace and publication

Project sessions require root-capsule `PinnedGeneration` plus `Cow` authority.
Workspace IDs and candidate rows are projections. Completion never publishes.
`validate-candidate-closure-and-base` proves canonical closure and admitted-base
ancestry only; project tests remain ordinary executions.

Publication additionally requires `ryeos.write.project.live`, expected base,
owner authorization, and HEAD CAS. A durable reservation and startup recovery
close the HEAD-before-root-fact crash gap. Root terminalization waits while
publication may have contacted HEAD. A process-local root-operation lease
fences every hosted root-chain mutation; terminalization closes admission and
waits on its condition variable rather than polling SQLite. Pinned CoW worker
executions admit exactly `retain_result`; projectless executions admit exactly
`any`. Discard/advance launch authority is not accepted by this release.

## Explicit non-claims

This substrate release does not provide hostile multi-principal containment,
provider-only egress, aggregate descendant quotas, worker pooling, invocation
leasing, cross-session reset, federation, or RyeOS local inference.

See also `knowledge:ryeos/core/kinds/worker` for the generic authored worker
kind and `knowledge:ryeos/standard/hosted-codex-activation` for the first
operator-facing structured-session integration.
