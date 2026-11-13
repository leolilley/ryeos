<!-- ryeos:signed:2026-11-13T02:54:45Z:7eadd55064c146d7d0b0c2f0c1cf1640dd67cc8e77f80fc29c4daa4bd5903288:CxKAj/jOvsJeuY+/CRW8o/AE8Q6DRRnlI+9l1l0HdRtLqGB/klfu3EBg5CHucWEjZuTrZR73DO9Pwm95kkBRDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/execution
tags: [execution, products, external-content, authority]
version: "1.0.0"
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

## Generation-scoped bindings and renewal

An installed-bundle binding names the consumer and its publisher, so it keeps
authorizing the consumer until it is released or its authorizer's grant
changes. A pinned-project binding also names the exact project snapshot and the
consumer's effective definition digest. A new project snapshot, or a re-signed
consumer, is therefore a new consumer authority: every earlier binding stops
authorizing it. This is deliberate. The node never carries a binding forward on
its own, and an earlier binding is never a fallback candidate.

Launch and qualification refuse such a consumer with code
`content_binding_missing` (HTTP 409). The refusal names the consumer, the
snapshot and effective digest it was resolved at, and each declaration with
its status (`missing_binding` or `missing_manifest`). Ordinary launch admission
reports the same condition as `external_content_binding_unavailable`.

Two local-operator verbs inspect and renew bindings:

```
ryeos external-content bindings <consumer-ref> <installed_bundle|pinned_project> [project-snapshot-hash] [--product-selections <json> --product-owner-principal <fp:hash>]
ryeos external-content rebind <prior-binding-hash> <project-snapshot-hash> [--product-selections <json> --product-owner-principal <fp:hash>] [--maximum-bytes <n>]
```

`bindings` is read only. It resolves the consumer at the named generation
through the same preparation bind uses, lists each of the consumer's binding
heads as `current`, `stale` or `released`, and gives the reasons a stale one no
longer applies (`project_snapshot_changed`, `consumer_definition_changed`,
`source_closure_changed`, `publisher_changed`, `consumer_kind_changed`,
`manifest_not_declared`, `authorizer_not_current`). For each retained
declaration it reports `ready`, `missing_binding` or `missing_manifest`, and
for a missing binding it names the newest stale pinned-project binding of the
same manifest as `prior_binding_hash`. Run it before a launch on a new
generation; offline doctor cannot decide this, because staleness depends on the
snapshot the launch picks.

`rebind` is the explicit, per-generation operator act. It accepts only an
active pinned-project binding that is the current head of its subject and was
authorized by the calling operator. Before anything is staged it prepares the
same consumer at the new snapshot and refuses, with code
`rebind_consumer_changed`, a consumer that has another publisher, no longer
depends on the project, or no longer declares the prior manifest. An
installed-bundle binding is refused with `rebind_not_generation_scoped`, and a
released or superseded one with `rebind_prior_binding_not_current`. It then
re-stages the prior binding's exact retained manifest (as
`import-binding` does) and binds it to the consumer at the new generation
through the ordinary bind, signed by the node and authorized by the operator.
The prior binding is left as it was. If the consumer already holds an active
binding at that generation the call returns it with `idempotent: true` and
writes nothing.

A consumer bound with product selections is renewed with the same complete
selection list and product owner it was bound with. `rebind` renews existing
bindings only; it does not import or compose products, so a product slot whose
witness changed still goes through `compose-product`.

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
