<!-- ryeos:signed:2026-11-13T03:56:20Z:b9a0c5bdd03f8e15ce1031f189d09157d52909e6c32fe31c0e15b23f217edf4d:ocxbQMjia9aaYVhw3iJANm2ujJ5VY6+eZGfzWmncIO9FGuKzWqzZmn1vdhb5sIWuiIA9/TRfN9VD95i512nfDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "remote-qualification-testimony"
title: "Remote Qualification Testimony: Design Rationale"
description: "Why a qualification proven on an origin node reaches a controller as a testimony (product lane at product selection, content lane against the receiver's own activation), what the receiver re-derives, and why the content lane is portable"
entry_type: decision
version: "1.1.0"
```

# Remote Qualification Testimony: Design Rationale

A controller must know that a worker runtime was proven safe before it
launches it. Some proofs can only be made on another node: a scope-empty
settlement needs a host with delegated per-scope cgroups, which a hosted
controller does not have. The origin proves it; the controller accepts the
origin's testimony under its own signed policy. The operator workflow is in
`knowledge:ryeos/core/execution/retained-products`. This entry records why
the crossing is shaped this way and what code changes must preserve.

Code: `crates/state/ryeos-state/src/external_content/products/qualification_testimony.rs`
(record kinds, acceptance policy, `admit`),
`crates/state/ryeos-state/src/external_content/qualification_subject.rs`
(the portable content identity),
`crates/daemon/ryeos-app/src/operator_external_content/qualification_testimony.rs`
(export, accept, selection and binding checks, typed content refusals) and
`crates/daemon/ryeos-app/src/operator_external_content/content_qualification.rs`
(the receiver's preparation and `validate_remote_purpose`).

## Doctrine

- Testimony stays testimony. The origin operator signs it over the origin's
  node-signed attestation. The receiver publishes its own node-signed
  acceptance in a separate namespace. Neither is ever published, loaded or
  counted as a local qualification, and a selection or binding must name a
  remote testimony explicitly.
- The receiver re-derives everything it can and records what it took on the
  attester's word. The lane's `*_LANE_REDERIVATIONS` and `*_LANE_ASSERTIONS`
  are written into every acceptance, so it never reads as more than it is.
  Both lanes take exactly one fact on the attester's word,
  `verifier_realization`.
- The receiver never runs the verifier and never reads the origin's
  execution as its own. It does not call `execution_evidence::verify_current`.
- What the receiver trusts (attester, origin site, claims, verifier digests,
  lanes, age) is signed acceptance-policy data, not code. A receiver that has
  not named an attester gains nothing from a testimony.
- No code names a product, provider or worker. The lane enum is the only
  closed vocabulary.

## Why the product lane

A five-claim worker qualification is product-lane by construction: its
launch is a product witness plus relationship plus consumer invocation, and
"remote consumer invocation requires a captured product". The worker's
runtime is a product slot whose profile says `runtime_authority:
captured_product`.

Two crossings were possible:

- **Content lane (rejected).** Re-express the proof over activated content.
  It needed a changed purpose invariant, a re-typed evidence join, a second
  worker and profile with `activated_content`, an activation for the runtime
  tree, and a node-independent content identity. It rebuilds the sandbox
  machinery for a second subject kind to carry a proof that already exists.
- **Product lane (chosen).** Let the existing record cross as a product
  testimony, consumed where the worker's runtime is admitted: product
  selection. No new evidence type, no purpose change, no second worker.

The state model already has a product-sourced program lane with its own
authority check, and the product testimony lane asserts almost nothing.

The five-claim worker still crosses on the product lane. The content lane is
now portable too (see "Portable content identity" below), so a runtime whose
program is activation-sourced crosses as a content testimony, consumed
against the receiver's own activation of the same signed release.

## Export re-verifies by lane

The origin signs only what it would itself select on now
(`export_testimony_blocking`). A policy with a consumer execution context is
re-verified the way product selection admits it
(`verify_current_product_qualification_for_export`): policy currency,
consumer content identity, verifier identity, the origin's own execution
evidence, and expiry. A policy without one is a guest-owner snapshot
qualification and keeps the snapshot check, which refuses consumer contexts
by design. Export requires the configured operator and refuses a
qualification without an expiry.

## Consumption at product selection

`ProductSelection.remote_testimony` is required-nullable. `None` is the local
path, unchanged. `Some` makes `qualification_hash` the origin's attestation
hash and requires `witness_source` to be `received`.

`verify_current_remote_selection_testimony` admits it when all of these hold:

- the attester is the selecting operator, and its grant and the origin
  node's grant are current;
- the acceptance policy resolves, allows the product lane, and is unchanged
  since the acceptance (`require_unchanged_since_acceptance`);
- the selection's received-product acceptance equals the product source the
  acceptance recorded, and the received product joins the evidence;
- the origin policy equals this node's current one for the same ref, and
  `admit` passes at the current time (attester and site, claims, verifier
  digests, age, skew, expiry);
- the evidence carries consumer content that matches the relationship's
  consumer;
- the verifier identity this node resolves from its installed bundle equals
  the evidence's digests;
- the consumer content closure is in this node's CAS, and the portable
  consumer identity equals this node's preparation.

The selection's received-product acceptance and the testimony acceptance are
two records in two namespaces. They are never equal; the receiver binds one
to the other through the acceptance's recorded product source.

### Portable consumer identity

`ProductQualificationConsumerContentIdentity` includes
`definitions.bundle_generation_identity`, which holds each bundle root's
filesystem path and cache identities. It differs on every node. Remote paths
compare `portable()`, which drops only that field. Local paths keep
whole-struct equality. The generation identity is an operational cache key,
not signed content, so nothing of proof value leaves the comparison.

### Verifier identity without the origin's capsule

The receiver uses `resolve_current_bundle_verifier_identity_against_admitted`,
which resolves the verifier Tool from the installed signed bundle by ref and
parameters. The capsule-based resolver would load the origin's sealed launch
capsule, which the receiver never holds. The origin verifier's realization is
not in the receiver's CAS either, so the product lane records
`verifier_realization` as an assertion.

## Placement follows the program's lane

The admitted program tags its runtime source `CapturedProduct` or
`ActivatedContent`. A product-sourced structured session leaves the
`runtime_content_qualification` slot empty: composition, placement and
recovery refuse it there. Its runtime authority is its admitted product
selection, which already carries the five claims on the same manifest the
session runs. A content qualification of that manifest would re-attest the
same claims through an activation the worker does not declare, and no
product-sourced session could ever meet it.

## Revocation

A revoked attester grant, a rotated key, a changed acceptance policy or an
expiry refuses every new composition and every new launch, because launch
re-runs the selection admission at dispatch. Recovery of an already
contacted session uses the retained acceptance and retained keys only.

## Immutability and age

- An acceptance is immutable at its coordinate within its evidence schema's
  head namespace. A second, different testimony or policy for the same
  coordinate is refused; re-qualification at the origin is the remedy, not a
  re-signed testimony.
- Editing an acceptance policy voids every acceptance made under it, even an
  edit that would still admit the testimony.
- `admit` checks the testimony's and the embedded qualification's age at
  every use, not only at acceptance. A testimony older than the policy's
  `max_testimony_age_seconds` (at most 31 days) refuses new selections and
  launches after a successful acceptance.

## Portable content identity

A content testimony speaks to activated content. Its sealed subject,
`ContentQualificationSubject`, names both the content and the records the
origin made of it. The receiver compares only the first part.

| Portable (`portable()`) | Why it is the same on every node |
|---|---|
| `activation_program_digest` | Hashes only signed data: the activation Config (sources with their HTTPS URLs and pinned archive and member `sha256`s, components and bounds), the consumer, and every `external_content` declaration of the consumer (pinned digest, mount, mount root, `qualification_allowance`). Node policy and the offline root stay out |
| `consumer_ref`, `declaration_id` | Signed names |
| `manifest_hash`, `manifest_kind` | The exact content tree, pinned by the signed declaration |
| `realization` | The signed mount and kind, and the metrics of the pinned manifest |

| Local join (`local_join()`) | Why it differs on every node |
|---|---|
| `activation_receipt_hash` | The receipt this node signed, naming this node |
| `binding_hash` | The binding for this node's own target and grant |
| `target_node_fingerprint` | This node |

Both projections are exhaustive destructures, so a field added to the
subject does not compile until someone decides which side it is on. Nothing
persisted changed for this: the subject schema, the content evidence and the
testimony body are as before, and the origin signs nothing new.

### What the receiver re-derives at every use

`verify_current_remote_runtime_content_testimony` (launch, and the
candidate-program emitter) does, after the acceptance and grant checks:

1. Derives this node's own admitted realization from its own signed
   activation component and resident manifest (`prepare_receiver_source`),
   never the origin's realization.
2. Runs the complete local join: its own node-signed activation head, the
   current active binding and authorizer, the manifest with its payload in
   CAS, the current signed allowance, policy and declaration authority, and
   its own verifier and producer-recipe resolution.
3. `validate_remote_purpose` requires this subject to equal the origin's in
   portable form and to name this node, then the same policy and allowance
   claims, the same producer recipes by signed content (without the
   node-local `bundle_generation_identity`, as `portable()` drops it for the
   product lane), and the same verifier definition digests.

Because `activation_program_digest` covers the activation's canonical ref,
the binding's `activation_ref` is rebound to the activation the origin ran at
every use. That is why the content lane no longer records `activation_ref` as
an assertion. The receiver supplies a freshly authenticated equivalent for
every field that left the comparison, so no implicit trust was added.

Local launch and admission keep `validate_purpose`, which compares the whole
subject.

### Typed refusals

Each way a receiver can differ is a `RemoteContentTestimonyRefusal` with a
stable code and a fix; the candidate-program API returns it as 409.

| Code | Seen | Fix |
|---|---|---|
| `remote_content_no_local_activation` | no completed activation of the program this node resolves | `ryeos external-content activate <activation_ref> ...` |
| `remote_content_identity_differs` | every differing portable field, both program digests and manifests | install and activate the release the origin qualified, or re-qualify at the origin against this node's release |
| `remote_content_policy_differs` | the policy ref, both effective digests and required claims | install the same signed policy and allowance, or re-qualify |
| `remote_content_producer_differs` | the scenario and both recipe content digests | install the same producer recipe, or re-qualify |
| `remote_content_verifier_differs` | both admitted and realized verifier digests | install the verifier the acceptance policy pins, with its pinned content resident and bound, or re-qualify |
| `remote_content_acceptance_superseded` | the acceptance's superseded schema | export the testimony again (below) |

The content comparison comes before the policy and verifier ones, so a
different release is named as such rather than by its consequences.

### Acceptance stays independent of activation

Acceptance does not resolve this node's activation: a node may accept before
it activates. `ryeos node doctor` reports each accepted content testimony as
`ok`, `no_local_activation` or `local_activation_differs`, read offline from
this node's signed heads, so the operator sees the gap before a launch
refuses.

### Retained by a session

A persistent-session capsule (schema 25) that runs on a remote content
testimony retains `receiver_join`: this node's receipt, binding and
fingerprint as its launch authenticated them. Its closure reaches that
receipt and binding, as a local qualification reaches its own through its
attestation's purpose. Recovery requires the join to name this node and the
receipt to be this node's record of the same program, consumer and
declaration binding.

## Acceptance evidence v4

Removing `activation_ref` from the content lane's assertions changed what an
acceptance states, so the acceptance evidence schema moved from v3 to v4 for
both lanes. A record's meaning must match its version. There is no
predecessor reader: a v1 to v3 acceptance is classified from its outer schema
and refused wherever it is read with `remote_qualification_acceptance_superseded`
(`remote_content_acceptance_superseded` on the content use path). That
includes a session capsule or product selection that retains one; a capsule
24 is in any case refused by the persistent-session schema check.

v4 heads live in `remote-qualification-acceptances-v4`. The v3 namespace
`remote-qualification-testimonies` is retired: nothing resolves an
acceptance through it, and the doctor lists its heads as superseded. Because
the namespaces differ, the cure is to export the same qualification again
from the origin (`ryeos external-content export-qualification-testimony`);
the receiver records a v4 acceptance at the same coordinate without
contradicting the old head. If the origin refuses the export because the
qualification is stale or expired, re-qualify there first. Then re-sign any
placement binding or product selection that named the old acceptance hash.
