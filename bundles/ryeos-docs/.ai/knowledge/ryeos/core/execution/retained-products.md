<!-- ryeos:signed:2026-11-13T03:56:20Z:29c8fd6eba27d0b76355b8234bbd734f0c159506230f3cec611f9afb93d47a51:JcFcBj9dzWaETmZK9dvRC5btKQuniUsRiRu/faSkHBWBUt7yN7TZ2owKW8TBYtVtgKGLPCb2eGyqYQVZKZn+Dg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/execution
tags: [execution, products, external-content, authority]
version: "1.1.0"
description: Exact ownership and node-local provisioning boundaries for retained build products.
---

# Retained products and operator authority

A signed producer recipe names bounded products. Successful retained execution
supplies their exact bytes and provenance; node-signed product testimony records
that evidence. A signed consumer relationship and, where required, an independent
qualification determine whether a selected product can supply a consumer slot.
Possessing bytes or completing a producer does not itself authorize consumption.

## One owner across production and consumption

Product capture, lookup, retained-product staging, qualification, selection and
composition admit either the configured local operator or an authenticated,
origin-bound configured remote operator. Each service still requires its own
capability. A remote-node grant is not an operator grant, and a remote operator
without verified source-node forwarding proof is not admitted.

Producer, witness, verifier, selected product and execution retain their exact
operator identity. Node identity signs testimony; it does not substitute the
target's local operator for the caller. There is no cross-owner product sharing
or implicit delegation. Separate nodes need not share private signing keys.

The target local operator can provision a producer's exact literal input
bindings. A remote operator then runs the producer on that target, qualifies its
own products and selects them for its own consumer. This composes existing
authority; it does not turn node-local provisioning into a worker permission.

## Boundaries which remain local

Named-root filesystem import, ordinary binding (including selected-literal
binding), managed activation, and node policy administration remain local
configured-operator operations. Product composition uses its own verified
product path into ordinary staging and binding; it cannot import an arbitrary
host path or bind caller-invented manifests.

Product operations preserve node capacity limits, pinned consumer source,
complete slot selections, signed relationships, qualification evidence and
exact owner checks. Active bindings retain the authorizer and exact grant
digest. Revoking or changing that grant invalidates those bindings, including
bindings authorized by a remote operator. Recovery uses retained owner/origin
authority and revalidates the current grant.

Target-local execution may retain a different `origin_site_id`: that records
the authenticated source, not a request to move an execution. Product admission
and recovery require the admitted `current_site_id` to match the serving node,
including selections on content dependencies, and verify exact local witness
or receiver-acceptance authority. They never rewrite the origin to pass a
locality check. A configured remote operator can submit selectors explicitly
for that destination; this does not resolve source-node selectors or transfer
products implicitly. Generic source-bound forwarding, worker handoff,
continuations, scheduled execution and candidate evaluation do not acquire
product-selection support from this destination-local contract.

Received product testimony is a separate same-operator contract. Trusting an
origin node does not transfer ownership or local qualification. A historical
witness from another operator must not be relabelled a local capture. Actual
large-object sidecar transfer remains separate from ordinary CAS-blob receipt.

For installed-bundle consumers, local literal provisioning is independent of
product selections because the binding names the consumer and publisher.
A pinned-project consumer containing both literal inputs and product slots on
that same item instead has a selection-sensitive binding identity. Its selected
literal provisioning currently requires the same operator as its products;
cross-owner selected-literal provisioning is not supplied by this contract.

## Using a qualification proven on another node

Some qualifications can only be made on another node. A scope-empty
settlement, for example, needs a host with delegated per-scope cgroups. The
origin qualifies the product and testifies to it. The receiving node accepts
that testimony under its own signed policy and selects the product with it.
The testimony is never loaded as a local qualification, and the receiver
never runs the verifier.

### Order of operations

1. Push the product from the origin to the receiver. The receiver records a
   received-product acceptance for that exact witness. The origin must
   qualify this same witness; a re-capture with the same content has a
   different witness hash and is refused at acceptance.
2. Qualify the product on the origin.
3. Install the acceptance policy on the receiver (below).
4. Export from the origin. Export is also the acceptance: there is no
   separate accept command on the receiver.
5. Select the product on the receiver, naming the acceptance.

### Export

On the origin, as its configured operator:

```bash
ryeos external-content export-qualification-testimony <remote> <qualification-hash> <acceptance-policy-ref>
```

The origin re-verifies the qualification the way it would select it now and
refuses a stale one or one without an expiry. It signs the testimony, posts it
to the pinned remote's `POST /external-content/accept-qualification-testimony`
route, and prints `testimony_hash`, `acceptance_hash`, `coordinate_id` and
`reused_existing`. Keep `acceptance_hash`; selections name it.

The receiver accepts only when the testimony's signer is the authenticated
caller and holds a current `remote_operator` grant, the named acceptance
policy resolves and allows the lane, the origin's qualification policy is the
same signed bytes the receiver resolves for that ref, and the qualified
product was received from that origin by that operator.

### The acceptance policy

An acceptance policy is a signed `config` item whose
`remote_qualification_acceptance_policy` field holds:

| Field | Meaning |
|---|---|
| `schema` | `ryeos.remote_qualification_acceptance_policy.v2` |
| `qualification_policy_ref` | the exact origin qualification policy ref this policy accepts |
| `trusted_attesters` | `{principal, origin_site_id}` pairs; both must match the testimony and the attester's grant |
| `required_claims`, `allowed_claims` | required must be a subset of allowed |
| `accepted_verifier_definition_digests` | the evidence's `verifier.effective_definition_digest` |
| `accepted_verifier_artifact_digests` | the canonical JSON digest of the evidence's `verifier.artifact_identity`, not the raw binary hash |
| `allowed_lanes` | `product`, `content`, or both |
| `max_testimony_age_seconds` | 1 to 2678400 (31 days) |
| `max_clock_skew_seconds` | at most 3600 |

Lists are sorted and unique. Because `qualification_policy_ref` must equal
the evidence's policy ref, each origin qualification policy needs its own
acceptance policy.

Finish the verifier digests before signing. Editing an acceptance policy
voids every acceptance made under it, even an edit that would still admit
the testimony. An acceptance is also immutable at its coordinate: a different
testimony for the same coordinate is refused, and re-qualifying on the origin
is the remedy.

Acceptances are recorded as acceptance evidence v4. An acceptance recorded
before v4 is refused wherever it is used, with the code
`remote_qualification_acceptance_superseded` (or
`remote_content_acceptance_superseded` when a content testimony is used). To
cure it, run the export above again from the origin; the receiver records a
new acceptance at the same coordinate. If the origin refuses the export
because its qualification is stale or expired, re-qualify there first. Then
re-sign every selection or placement binding that named the old acceptance
hash with the new one. `ryeos node doctor` lists superseded acceptances under
`remote_content_testimonies`.

Age and expiry are checked at every use, not only at acceptance. Selection
and every launch re-run them against the current time, so a
testimony older than `max_testimony_age_seconds` stops new launches even
after it was accepted.

Ship the policy as a small downstream bundle signed by a publisher the
receiver trusts. A `config:<dir>/<id>` ref names a directory, not a bundle,
so use a directory no other installed bundle uses. Install it with
`ryeos bundle install <name> <dir>` on a stopped node.

### Selecting the product

A product selection names the remote testimony explicitly:

```json
{"declaration_id": "guest-runtime",
 "witness_hash": "<received witness>",
 "witness_source": {"kind": "received", "acceptance_hash": "<received-product acceptance>"},
 "qualification_hash": "<origin attestation hash>",
 "remote_testimony": {"acceptance_hash": "<testimony acceptance>",
                      "acceptance_policy_ref": "<acceptance policy ref>"}}
```

`qualification_hash` and `remote_testimony` are required-nullable keys:
always present. The two acceptance hashes are different records.

The receiver admits the selection only for the attesting operator, who also
owns the received product. From the origin that is `ryeos remote run
<receiver> service:external-execution/admit-product-program
--outbound-principal configured_operator ...`, and the origin operator's
grant on the receiver needs
`ryeos.execute.service.external-execution/admit-product-program`.

`admit-product-program` derives the admitted program for a captured-product
worker and returns `program`, `selections_digest`, `runtime_manifest_hash`
and `selection_identity_digest`. It writes nothing.
`ryeos node external-execution compose --program` reads the bare `program`
object, not that envelope. A product-sourced program carries no runtime
content qualification: pass `--runtime-content-qualification none`.

### Using a content testimony

A runtime that is activated content rather than a captured product crosses on
the content lane. The receiver does not take the origin's activation on
trust: it activates the same signed release itself, and every use compares its
own activation with the content the origin qualified. Only the content's
identity is compared: the activation program digest (the signed activation,
its pinned archives and the consumer's declarations, including mounts), the
consumer, the declaration, the exact manifest and its realization. The
origin's receipt, binding and node are its own records and are never
compared.

1. Install the same signed Bundle release on the receiver as on the origin,
   including the qualification policy and the verifier.
2. Install a content acceptance policy whose `allowed_lanes` includes
   `content`.
3. Export from the origin with `--content '{"activation_ref": ...,
   "coordinate_id": ..., "manifest_hash": ...}'`. The receiver may accept
   before it has activated.
4. Activate on the receiver: `ryeos external-content activate <activation-ref>
   <online|offline> [offline-archive-root]`. Keep the verifier's pinned content
   resident and bound on the receiver, as for a local qualification.
5. Emit the program and binding selection as the attesting operator:
   `ryeos external-execution admit-candidate-program <activation-ref>
   <coordinate-id> <attestation-hash> <runtime-manifest-hash> --required-claims
   <json> --remote-testimony '{"acceptance_hash": ..., "acceptance_policy_ref":
   ..., "owner_principal": ...}'`. Compose the placement binding with the
   returned `runtime_content_qualification`, which carries `remote_testimony`.
6. Read `ryeos node doctor`: the `remote_content_testimonies` row reports each
   accepted content testimony as `ok`, `no_local_activation` or
   `local_activation_differs`.

Launch and the emitter refuse with a typed code that names what differs and
its fix: `remote_content_no_local_activation`,
`remote_content_identity_differs` (naming each differing field, such as
`activation_program_digest`, `manifest_hash` or `realization`),
`remote_content_policy_differs`, `remote_content_producer_differs`,
`remote_content_verifier_differs` and `remote_content_acceptance_superseded`.
A different release on the receiver needs either the origin's release or a
re-qualification at the origin against the receiver's release.

## Evidence is not task success

Product qualification establishes the claims required by its signed relationship.
It does not qualify future worker edits. Worker command completion, frozen
candidate capture, independent candidate evaluation and explicit publication
remain separate authorities and qualification gates.

For an external candidate runtime, the qualification policy's sealed verifier
parameters must also contain the typed external-candidate use context. That
context commits the full validated consumer execution requirement, including
its runtime command, environment, containment and resource bounds. Admission
compares the context with the current requirement before accepting the selected
product. A changed requirement needs a qualification for that exact new use;
changing the selected execution identity alone is not a qualification test.
The generic product qualification service retains these parameters as signed
opaque input and does not interpret the external execution protocol.
The controller provider profile, executable and configuration source identities
require their own exact comparison with the tested tuple before activation.
