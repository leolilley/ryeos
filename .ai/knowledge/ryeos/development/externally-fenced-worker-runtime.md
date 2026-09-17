<!-- ryeos:signed:2026-09-17T02:55:59Z:b455a2a829bb29dcc7836abb6b3b7ceb38abe324adb38b14f8dae22407c43041:J7aZYxPJExBakBWTGDHLUZ2A4GgyNTfob3f2NX9/EEDFOi+I3Le90Qi0D/5SkY9tNj1kKT9iTFDAqY2oBSSgBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/development
name: externally-fenced-worker-runtime
title: Externally fenced worker runtime
description: Generic authority and qualification boundary for single-tenant externally supervised worker sites
entry_type: implementation_guide
version: "1.1.0"
```

# Externally fenced worker runtime

An externally fenced worker site is one ordinary configured RyeOS remote whose
complete execution incarnation is owned by an independent deployment
supervisor. It is not a Lillux host and not an alternative worker protocol.
Each site owns one node identity, private persistent app-root state,
target-local credential profiles, launch recovery state, and retained candidate
evidence. The existing remote-worker workflow transfers the exact project,
reconciles one launch, and returns the existing target-signed candidate result.

Projects may select or narrow a named protected lifecycle profile, slot
requirement, and budget. Exact provider service IDs, regions, image and public
configuration ceilings, lifecycle bindings, credential handles, and mutation
authority belong to protected source-node installation state. They are not
project authority or RyeOS development constants. Railway, Fly, Modal, a
managed container service, or a future deployment backend can bind this
contract only after its installed adapter and evidence satisfy the same generic
authority requirements.

## Distinct runtime products

A general hosted workflow-node image may coordinate work without claiming hard
worker containment. A hard-contained OCI worker is a separate product with an
administrator-prepared Lillux binding and signed required-isolation profile.
An externally fenced restricted worker is a third product: it uses independent
site-lifecycle fencing and a closed model-tool inventory. No product may claim
the authority of either other product merely because it contains the same
RyeOS binaries or bundle inventory.

The ordinary hosted-authoring worker gives a model a shell and file-editing
loop. It is admissible only under independently qualified containment. An
external restricted profile is admissible only if startup evidence proves a
closed tool inventory that excludes built-in shell, unified execution,
patch/file mutation, browser, MCP, plugin, and every other ambient execution
route. Project-owned admitted RyeOS operations may then provide bounded editing
and verification. A feature flag, prompt instruction, self-reported tool list,
or failed shell command is not qualification evidence. Until this gate is met,
launch must refuse before model-provider turn contact.

## Authority split

The source workflow owns configured-remote selection, exact workflow intent,
aggregate concurrency policy, and recovery. Project configuration may select
or narrow one named protected lifecycle profile, slot requirement, and budget;
it cannot supply authority-bearing provider coordinates to broad credentials.
The source-node operator installation owns the adapter binary and content
identity, NodeVault credential handle, provider account/project ceiling,
allowed slot identities, expected image/configuration, and permitted
operations. The deployment operator provisions or revokes that protected
binding and retains explicit break-glass authority. Each target owns its node
identity, credential-profile home, provider session, target launch, private
candidate, and candidate testimony. Lifecycle credentials never enter a
target or project generation.

Four terminal facts remain separate:

1. The provider turn and exact hosted command completed.
2. Writers were excluded and the target signed the frozen candidate result.
3. The exact frozen candidate closure was imported into source-retained CAS
   without applying or publishing it.
4. The external placement occurrence ended and its slot became reusable.

The target can prove the first two. The source import path proves the third.
The target cannot prove its own future death or
safe replacement. `service:node/status` therefore cannot satisfy external
placement-incarnation cleanup, even if it returns plausible provider IDs or
digests. The installed adapter must retain an independent lifecycle release
receipt and compose it with, never replace, provider completion, candidate,
and source-import receipts.

## Provider adapter contract

The provider adapter controls only preconfigured site lifecycles. It does not
transport projects, launch provider turns, reconstruct candidates, sign project
content, or publish results. Its protected binding names the exact provider,
account or project boundary, configured remote site, expected image and public
configuration, and permitted lifecycle operations. Credentials remain in the
source node's private authority.

The adapter reconciles a durable mutation intent and returns an exact observed
placement occurrence. The source first pushes the project to the target, which
prepares and signs exact launch admission without starting a worker or
contacting the model provider. The controller verifies the prepared capsule,
exact program, retained session protocol, target key, and authenticated boot,
then composes and issues a one-use lifecycle admission grant from the protected
adapter evidence. The grant binds the adapter and binding generation,
configured remote and slot, source site/work/operation, prepared target launch,
provider-observed placement occurrence, expected image/configuration,
observation generation, and cleanup requirement. The target atomically
consumes it for that exact prepared launch before starting the worker. A
general statement that a site is externally fenced is not launch authority.

Grant expiry gates first consumption only. After durable consumption, the
placement reservation and cleanup obligation remain active until an exact
release or quarantine settlement. Expiry cannot orphan a running placement or
authorize another grant for its unresolved slot.

The provider evidence remains controller-observed correlation unless the
provider supplies independently verifiable attestation. Boot binding requires
a controller-minted one-use challenge answered under the pinned target key;
a deployment identifier echoed by the target is insufficient.

Unknown mutation contact remains unknown. RyeOS reconciles the exact provider
operation before retrying and quarantines the slot when absence or termination
cannot be proved. A new campaign attempt may intentionally repeat provider work
under a new identity; it is not replay and must not be described as proven
non-contact.

Successful-candidate replacement cannot release capacity until the candidate
closure is durable in source CAS and the adapter establishes that the old
placement occurrence is fenced and no stale target can authenticate as the
current slot. A failed or cancelled launch may instead settle as
`candidate_absent`: a typed non-publishable terminal or lost disposition with
provider-contact accounting and no candidate-result claim. Ambiguous outcomes
remain quarantined. Candidate capture alone does not release the site or
credential profile. Equivalent recorded workflow replay performs no lifecycle
mutation and no model-provider contact.

The durable phase order is:

```text
lifecycle intent
→ allocation reconciliation
→ authenticated boot binding
→ target readiness and project push
→ target-signed prepared launch admission without worker/provider contact
→ source verification and lifecycle admission grant
→ atomic grant consumption and target launch
→ provider completion and frozen candidate
→ source CAS import or typed candidate-absent disposition
→ release intent
→ placement fence/release proof or quarantine
→ final composed receipt
```

External cleanup is only a cleanup fallback. Admission also requires exact
filesystem realization, a closed model-tool inventory, supervisor and
credential separation, and writer-exclusion qualification. A lifecycle grant
cannot waive any of those gates.

## Credential and command boundary

Every site uses an independently enrolled target-local credential profile.
Private credential homes are never copied from the controller or between
sites. Profile locks and subject identity continue to use existing RyeOS
credential/session owners. Subscription concurrency ceilings are enforceable
policy; provider-reported usage is observation, not an authoritative monetary
ledger.

Even a restricted profile keeps node identity, lifecycle authority, controller
channels, result-import grants, signing material, other candidates, and
persistent admission state outside the model-command trust domain. A model
process receiving a durable personal-account refresh credential has durable
account-compromise potential, not merely temporary runtime access.

## Staged acceptance

1. Validate the closed schemas and refusal-before-contact behavior.
2. Exercise a fake lifecycle adapter with contact counters across stale
   placement occurrence, ambiguous mutation, overlapping replacement, crash before
   capture, crash before release, and duplicate acceptance.
3. Run a synthetic non-secret worker on one configured destination.
4. Qualify command and credential separation without production credentials.
5. Run one explicitly authorized live worker candidate.
6. Enroll a second independent site and prove aggregate concurrency limits.
7. Run a project base-to-candidate-to-independent-evaluation workflow.

No stage grants publication, project signing, competition submission, node
lifecycle, or general daemon authority to the worker. A passing candidate is
retained for the existing independent evaluation and integration process.
