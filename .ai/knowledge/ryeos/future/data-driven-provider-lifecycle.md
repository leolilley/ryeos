<!-- ryeos:signed:2026-10-06T01:00:58Z:bafa51a0feaba6e8a173a74d462ae8ea4cb3608438b90b4867b251bde43dd12f:JuHCLALid90X9vr5hu0wRKMGXU8UbvsvBU29XeScjY8tQZ8Ly1aPEsHjI7LPaLA6dlcjSCcdGDbXKKiLpjIcDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: data-driven-provider-lifecycle
title: Data-Driven Provider Lifecycle Operations
description: Future separation of provider lifecycle profiles, shared bounded interpretation, and native host mechanisms
entry_type: design
version: "0.1.1"
```

# Data-Driven Provider Lifecycle Operations

## Status and ordering

Deferred shared-profile design, recorded on 2026-09-28 and reconciled with the
[platform direction](platform-direction-and-delivery.md) on 2026-10-06.
This note owns finite declarative lifecycle profiles. The [integration-adapter
owner](language-neutral-integration-adapters.md) owns admitted procedural product
implementations when profiles cannot express their behavior.

Keep current launch, authenticated readiness, settlement, recovery and frozen
result qualification requirements intact. A second real provider/profile and
operation evidence gate extraction of a shared lifecycle interpreter. A second
verification scenario separately gates shared verification abstractions. Neither
gate requires ordinary client/authoring work to wait for both generalizations,
nor makes an already-supported interpreted implementation depend on them.
This design does not authorize provider contact or claim operational acceptance.

Do not combine the verifier and lifecycle adapter into one universal plugin
system. They interpret different protocols and produce different evidence.
Their shared principle is that signed data selects bounded, already-supported
behavior while small trusted interpreters implement its semantics.

The Render integration worktree currently provides a concrete design example:
`render-sandbox-adapter` has a signed provider-spec artifact and a narrow Rust
interpreter. A proposed `snapshot_production.rs` adds careful Render-specific
snapshot response handling. It is useful evidence for this future design, not
an established platform contract. Recheck current source and worktree status
before implementation.

## Goal

Provider lifecycle differences should usually be expressed as signed,
versioned profile data interpreted by a shared bounded runtime:

```text
signed provider profile + admitted operation intent
  -> validate profile and operation against a finite interpreter vocabulary
  -> construct the provider request through the approved host transport
  -> parse the bounded response without ambiguity
  -> bind provider observations to the exact operation intent
  -> emit a generic lifecycle result for independent qualification
```

Adding or changing a provider's supported route, response-field mapping, or
status mapping should not ordinarily require a new provider-specific Rust
implementation. RyeOS should use the same pattern it uses for LLM providers:
provider configuration selects a supported profile; an admitted interpreter
supplies its semantics through the shared host and evidence contracts. Procedural
behavior outside the finite profile vocabulary belongs in separately admitted
implementation code, which may use Python or another supported runtime.

## The present hard-coded example

The proposed Render snapshot module contains two kinds of logic:

1. **Provider protocol details:** Render's response field names, snapshot
   `kind` and `status` strings, and the accepted HTTP status. These are
   candidates for the signed Render profile.
2. **RyeOS trust invariants:** response size limits, strict parsing, exact
   source/group/plan comparison, timestamps, canonical identities, operation
   binding, and the rule that a create response is only a provider locator—not
   proof of installed contents. These belong in reviewed shared code or the
   generic operation contract.

Keep the second category enforced by code. Moving the first into data must not
turn the provider profile into an unrestricted HTTP script or make its response
claims authoritative.

## Proposed responsibility split

### Signed provider profile

A provider profile may describe only the finite facts needed to interpret a
supported lifecycle operation, such as:

- an admitted provider identity and supported protocol/profile version;
- a named route assembled from fixed segments and admitted bindings;
- an operation kind selected from the runtime's known operation vocabulary;
- request-field projections from typed settings and operation intent;
- the provider's accepted response code class and bounded response shape;
- mappings from provider response states to generic states such as pending,
  available, or failed;
- field mappings for the provider locator and exact source identity; and
- a named proof profile whose semantics are implemented and reviewed in code.

The profile is signed and digest-bound to the adapter/runtime generation, as
with the existing provider-spec handoff. Configuration remains data; it does
not execute itself or create capabilities.

### Shared lifecycle interpreter

The interpreter validates the signed profile and implements the finite
operations that multiple providers actually share. It should:

- accept only supported protocol/profile, operation, route, field-source, and
  proof identifiers under admitted provider and network authority;
- validate the profile against its schema and settings-schema digest before
  network contact;
- construct requests only through the approved Lillux host transport and
  admitted origin/credential rules;
- enforce operation deadlines, cancellation, byte bounds, strict duplicate-
  rejecting parsing, and no-retry rules for uncertain non-idempotent contact;
- map a parsed provider response into a generic, operation-bound observation;
- preserve pending, failed, and uncertain outcomes instead of guessing success;
  and
- refuse unknown fields, unsupported profile versions, unknown status mappings,
  and unrecognized provider behaviors before contact.

The interpreter is deliberately not a general expression engine. Do not allow
arbitrary URLs, methods, scripts, JSONPath, shell commands, retry programs,
credential destinations, or profile-authored proof logic. Extend its vocabulary
only when a concrete provider needs the behavior and the security boundary has
been reviewed.

### Generic operation intent and evidence

The provider-neutral operation intent owns RyeOS facts: operation identity,
selected retained product, expected runtime manifest, controller trust root,
source occurrence, and relevant resource limits. These facts are authorized by
RyeOS admission and must not be supplied or rewritten by provider response
mapping.

The generic result should say what the provider observed and bind that result
back to the exact intent. For snapshot production, it may identify an opaque
provider snapshot locator and the exact response digest. It must not claim the
snapshot contains the expected product merely because create was accepted or
the provider reports `available`. A separate restored-guest verifier must
observe the snapshot's exact runtime tree and join it to the retained product
and qualification policy.

### Lillux and adapter boundary

Lillux continues to own host mechanisms: bounded byte I/O, TLS/network hooks,
monotonic deadlines, cancellation and settlement. It does not interpret Render
JSON, lifecycle states, snapshot identity, or RyeOS qualification claims.

The generic lifecycle runtime uses those host mechanisms. Provider profile data
describes the supported API variation. The operation owner joins the
interpreted response to the admitted intent and durable lifecycle record.
This keeps provider-specific parsing out of Lillux and avoids a separate large
Rust response interpreter for every provider.

## Relationship to the LLM provider pattern

The useful analogy is the separation between provider/model configuration and
the compiled provider client:

| Provider lifecycle | LLM provider pattern |
| --- | --- |
| Signed profile selects a supported lifecycle protocol | Provider/model configuration selects a supported route |
| Shared interpreter builds bounded lifecycle requests | Shared client builds provider requests |
| Provider-specific response mappings are declarative data | Provider-specific model and endpoint settings are data |
| Rust enforces identity, transport, deadlines, and evidence joins | Rust enforces credentials, transport, usage, and result handling |
| Independent qualification checks the restored runtime/product | Evaluators interpret the returned model result for their task |

The mapping is architectural, not an instruction to reuse LLM request/response
types for lifecycle APIs. Snapshot production, sandbox allocation, activation,
termination, and LLM inference are distinct operation families with distinct
trust and recovery semantics.

## Build and development impact

Once the generic interpreter supports an operation, profile-only changes can be
reviewed and tested as signed data without editing or recompiling the adapter's
provider logic. They still require the normal bundle-generation, signature,
admission, and qualification steps. Changing interpreter behavior requires
the checks appropriate to its admitted implementation. A missing shared host
mechanism or changed native guarantee requires a Rust release; procedural
provider semantics do not inherently require Rust. Never embed arbitrary scripts
inside the restricted profile language to avoid that distinction.

To keep development feedback fast:

- exercise profiles with a lightweight schema/fixture validator before a full
  workspace build;
- keep one shared adapter/interpreter binary and run it against multiple
  signed-profile fixtures;
- add negative profile tests for unsupported origins, methods, response codes,
  state mappings, and ambiguous or oversized responses;
- re-sign changed profile definitions and repeat the admission/qualification
  invalidated by their exact identity; reserve native rebuilding for changed
  executable semantics; and
- do not weaken profile validation or skip qualification to avoid a slow build.

## Adoption sequence

For shared profile-interpreter extraction, first select working concrete
operation contracts and a repeated provider requirement. Coordinate changes
with the current integration owners and preserve their acceptance. A generic
verifier framework is not a prerequisite for this separate lifecycle contract:

1. Inventory lifecycle operations used by the current Render route and one
   concrete second provider or materially different profile. Separate provider
   protocol differences from shared authority and recovery rules.
2. Extract only the provider-neutral intent/result facts and strict parsing
   primitives that those operations share. Keep the existing Render path
   working during the extraction.
3. Extend the signed provider-spec schema with one bounded operation at a time,
   starting with the repeated lifecycle operation that has the clearest
   operation-intent binding. Snapshot production is a candidate only after its
   source, pending/available, and restored-content evidence semantics are
   specified.
4. Convert the Render-specific response structs and status mappings into the
   signed profile. Keep response parsing strict and compare mapped values
   against the admitted operation intent through its shared authoritative owner.
5. Run the same interpreter tests against Render and the second profile. Prove
   profile substitution, response tampering, wrong source occurrence, missing
   metadata, duplicate keys, oversized bodies, cancellation, timeout, and lost
   response all fail closed or remain explicitly uncertain.
6. Independently restore and inspect each qualified snapshot. A provider
   locator, API response, or `available` state alone never qualifies installed
   contents.

Do not build a multi-provider abstraction because two Rust files look similar.
Require a second real profile to demonstrate what can actually be shared.

## Acceptance criteria

- Provider/profile bytes are signed, canonical, bounded, digest-bound, versioned,
  and reject unknown or ambiguous fields.
- Profiles select only reviewed protocol operations, route forms, field sources
  and proof profiles under independently admitted origin/credential authority.
  The current closed Render profile stays closed until an explicit migration.
- No profile can add a credential route, arbitrary network destination,
  executable logic, retry, or qualification capability.
- Provider response parsing is bounded and rejects duplicate keys, trailing
  bytes, unknown required semantics, malformed identity, and invalid state
  transitions.
- Every provider observation is joined to one admitted operation intent,
  source occurrence, and operation deadline.
- Accepted provider work is never mistaken for command completion, resource
  readiness, restored content, or qualification.
- Cancellation, provider uncertainty, restart, and reconciliation retain their
  exact lifecycle meanings and cannot duplicate non-idempotent effects.
- A provider-profile-only change exercises the existing interpreter without a
  provider-specific Rust branch.
- A second concrete profile demonstrates reuse before the shared runtime is
  declared a stable platform contract.

## Related knowledge

- [Data-Driven Runtime Verification](data-driven-runtime-verification.md)
  describes the separate future for scenario policy and runtime protocol
  interpretation.
- [Nested Execution Ownership and Evidence](nested-execution-ownership-and-evidence.md)
  records why execution ownership, protocol interpretation, and evidence
  authority remain separate.
