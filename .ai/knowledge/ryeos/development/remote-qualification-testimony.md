<!-- ryeos:signed:2026-11-12T23:57:18Z:6460fec0f8b4318247a4ccb2d6cbd11708fa0ae2926ab54d607cd90167e0f5c5:qrFyu8NcpBS46680LEatx/m9I2Ph7XH35EQ3Is2oPet54Yk8/MSqosHubg5+OAEUvm1NXJKT6MJhij8SeyETDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "remote-qualification-testimony"
title: "Remote Qualification Testimony: Design Rationale"
description: "Why a qualification proven on an origin node reaches a controller as a product-lane testimony consumed at product selection, what the receiver re-derives, and the known limits"
entry_type: decision
version: "1.0.0"
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
(record kinds, acceptance policy, `admit`) and
`crates/daemon/ryeos-app/src/operator_external_content/qualification_testimony.rs`
(export, accept, selection and binding checks).

## Doctrine

- Testimony stays testimony. The origin operator signs it over the origin's
  node-signed attestation. The receiver publishes its own node-signed
  acceptance in a separate namespace. Neither is ever published, loaded or
  counted as a local qualification, and a selection or binding must name a
  remote testimony explicitly.
- The receiver re-derives everything it can and records what it took on the
  attester's word. `PRODUCT_LANE_REDERIVATIONS` and `PRODUCT_LANE_ASSERTIONS`
  are written into every acceptance, so it never reads as more than it is.
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

- An acceptance is immutable at its coordinate. A second, different
  testimony or policy for the same coordinate is refused; re-qualification at
  the origin is the remedy, not a re-signed testimony.
- Editing an acceptance policy voids every acceptance made under it, even an
  edit that would still admit the testimony.
- `admit` checks the testimony's and the embedded qualification's age at
  every use, not only at acceptance. A testimony older than the policy's
  `max_testimony_age_seconds` (at most 31 days) refuses new selections and
  launches after a successful acceptance.

## Known limit: remote content testimony is node-bound

The content lane can export and accept a testimony, but a receiver cannot use
one for a different node's activation. `verify_current_remote_runtime_content_testimony`
re-prepares the receiver's own activation and calls `validate_purpose`, which
requires the prepared `ContentQualificationSubject` to equal the origin's
(`content_qualification.rs`). That subject carries
`activation_receipt_hash`, `binding_hash` and `target_node_fingerprint`,
which are node-specific. Making the content lane usable across nodes needs a
node-independent content identity for the remote path, as `portable()` does
for the product lane. No shipped bundle declares a `qualification_allowance`
today, so nothing produces a content-lane qualification to carry.
