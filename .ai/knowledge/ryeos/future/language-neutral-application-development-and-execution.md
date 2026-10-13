<!-- ryeos:signed:2026-10-06T01:00:58Z:3545021904afb4535376bca2885bd460f464f6752063a207ee7951263463d998:04NXSnx30hsMDf/2prZmMaUtbjid5SCpuKvp68UsctTCDd606Ohg/4gr/WZw4hM3mDQY8ZW2LAuanm3EK+4pCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: language-neutral-application-development-and-execution
title: Language-Neutral Application Development and Execution
description: Author and drive RyeOS applications from other languages through existing execution, content, authority and lifecycle contracts
entry_type: design
version: "0.1.1"
status: discussion
```

# Language-Neutral Application Development and Execution

This note owns authoring/control contracts and SDK staging beneath
[Platform direction and delivery](platform-direction-and-delivery.md), which
owns the overarching application lifecycle and platform milestones.

## Purpose and status

RyeOS should be usable as an execution platform by applications written in
Python, TypeScript and other languages. Authors should be able to implement
capabilities and invoke existing capabilities without rebuilding unrelated
Rust foundations. This includes ordinary tools, managed workers, declared
workflows, model-driven work, admitted runtime/content consumption and supported
remote execution.

This note records the discussion of 2026-10-06. It defines a developer-facing
direction and staged implementation questions, not a delivered SDK, an
implementation allocation or a claim that current integrations are qualified.
The active Render/Codex and Modal work retains its existing scope and owners.
Finishing native startup and ownership does not automatically implement the
client APIs, TypeScript runtime or general delegation described here.

The central requirement is a coherent authoring and control experience over
existing RyeOS contracts. Keep their different authority and lifecycle meanings
visible. No new universal execution protocol, kind, scheduler, content store or
service registry is proposed.

## Two ways to participate

1. **Author capabilities inside RyeOS.** Package tools, workers, adapters,
   semantic checkers, graphs, directives, configurations and views. Select the
   implementation's admitted source and runtime/dependency closure.
2. **Drive RyeOS from another application.** An authenticated client invokes
   permitted items/services, observes work and uses supported lifecycle
   operations. A Python backend, TypeScript web interface or command-line
   program can be a caller without becoming a hosted RyeOS workload.

Hosted code may itself make permitted platform requests through the applicable
callback/workload-client contract. That is delegated execution under a specific
grant, not an ambient connection with all daemon or parent authority. An external
client key, browser session and hosted callback token are different identities
and authority mechanisms; an SDK must not substitute one for another.

Launch admission and runtime authority are separate: `required_caps` specifies
what a caller must possess to invoke an item, not what that item receives.
Direct-tool resource grants follow verified manifest/request rules. Some existing
thread-local callbacks instead use their token and method access class. Inventory
both surfaces rather than assuming one uniform capability rule for all callbacks.

## Existing foundation and implementation gaps

These are source/documentation observations. Actual availability depends on the
installed composition, selected node policy and qualified execution arrangement.
Recheck operating contracts when allocating implementation.

| Surface | Existing basis | Remaining developer-facing work |
| --- | --- | --- |
| Python tools | Function/script runtimes, parameters, source capture and results | Reusable authoring harness and explicit dependency/runtime packaging |
| External control | Authenticated `/execute`, `/execute/stream` and service descriptors | Maintained client libraries, cancellation/reconnect contracts and error types |
| Callbacks | Specific controlled tool callbacks and hosted workload clients | Map exact supported operations; broader nested invocation needs explicit delegation work |
| Graphs/directives | Existing signed definitions and runtimes | Typed authoring conveniences without a parallel evaluator |
| Persistent workers | Worker/session contracts, commands and lifecycle ownership | Product-neutral ergonomic clients with accurately declared capabilities |
| Environments/content | Pinned external content, exact realizations and environment selection | Convenient consumption, availability/error reporting and supported artifact publication interfaces |
| Remote execution | Explicit remote operations and selected hosted-worker placement/checkpoints | Expose actual placement capabilities without inventing automatic scheduling or live migration |
| TypeScript implementations | Language-neutral wire contracts | Select and qualify a concrete JS runtime, source/build-output/dependency arrangement; broad support is not established by Python support |
| In-process services | Native registry-backed `service` endpoints | Interpreted code is not automatically an in-process service; use existing tools/workers or separately justify a hosting contract |

Current broad SDK examples must not be presented as runnable repository APIs.
In particular, a general `ctx.execute()` for every tool and a generic opaque-byte
artifact-ingest callback are not established interfaces. The Python runtime
contract explicitly records the latter gap.

## Execution range and capability accuracy

| Execution form | Developer use | Meaning the API must preserve |
| --- | --- | --- |
| Tool | Bounded transformation or operation | Invocation result, effects and retained outputs remain distinct |
| Graph | Declared composition, branching and supported retries | Runtime-owned graph state and failure/recovery semantics |
| Directive | Model-driven work with selected providers/tools | Provider observations, grants and accounting under the actual contract |
| Fixed persistent worker | Repeated requests or application interaction | Process instance, request outcome and session termination are different facts |
| Hosted worker execution | Workspace-backed interaction and supported candidate operations | Exact environment, credentials, placement/boot identity and disposition authority |
| Future leased invocation worker | Reuse a warm runtime for authorized invocations | Separate lease admission/reset/cancellation; existing fixed workers are not general invocation vessels |
| Environment production | Assemble and verify runtime/dependency content | Production, qualification, publication and consumer binding have separate owners |
| Explicit remote placement | Execute at a supported selected site | Target admission and resource ownership; no promise of arbitrary backend equivalence |
| Supported checkpoint/continuation | Continue from retained state | Cold restoration under the selected contract, not migration of arbitrary live processes |
| Future training/specialized workloads | Consume corpora/models/programs and retain candidates | Selected resource, numerical, privacy and evaluation contracts; no generic sealed-result claim |

Provide common vocabulary where meanings agree. Keep typed operation-specific
handles and capabilities where they differ. Do not normalize unsupported
rollback, checkpointing, streaming or cancellation into apparent success.
Discovery should describe supported operations, version/profile and current
availability. Admission remains authoritative at invocation: discovered support
does not confer a grant or guarantee resources remain available.

## Environment, realization, isolation and placement

These are separate responsibilities:

- **Environment production** creates and verifies content for publication.
- **Realization** materializes particular admitted content as files/resources.
- **Environment selection** binds a workload to its required content and
  process configuration.
- **Isolation** controls how the actual execution accesses its workspace,
  content, channels and host resources under node-owned policy.
- **Placement** chooses a supported execution site and carries the required
  admission/content/continuation relationships to that site.

A hash identifies content; possession of it is not read or execution authority.
A realized file is not evidence of containment or compatible runtime behavior.
Provider hosting is not the isolation policy inside the guest. Host capabilities
and outer resource/tenant containment need their own applicable contracts.

The desired consumer path is:

```text
authorized caller invokes a selected definition
  -> resolve and admit exact code/environment/content bindings
  -> prepare resources at the supported selected target
  -> materialize verified content under the target's authority
  -> launch through the applicable execution and isolation contract
  -> retain progress, results, effects and lifecycle observations
  -> publish/dispose/reconcile through their actual owners
```

That sequence is conceptual; provider preparation ordering belongs to the
actual operation contract. Materialization must not contact an external provider
before its required intent, funding or contact authority is established.

Environment selection belongs to typed definition/runtime bindings, not arbitrary
payload strings. A portable worker's existing `environment` binding is not an
ordinary input parameter. Signed definitions do not widen immutable node policy;
caller overrides require explicitly supported admission rules.

Source, interpreter, stdlib, packages, native extensions, loaders and applicable
external resources must have the identity/custody required by that execution.
Host PATH, mutable names and live virtualenvs cannot silently substitute for
exact dependencies. Model files and datasets are content dependencies with
their own access, bounds and retention; they are not necessarily executables.

Consumption should not require copying a producer or understanding its archive
layout. Reuse authoritative realizations and verified caches without weakening
consumer binding. Changing admitted environment bytes changes the relevant
execution identity. Recovery retains original bindings rather than resolving a
mutable name to its latest version.

## Illustrative development experience

Only the Python function entry point here is an existing documented interface.
Its surrounding definition selects the runtime and source/dependencies:

```python
def execute(params, project_path):
    readings = params["readings"]
    return {"count": len(readings), "maximum": max(readings)}
```

The runtime serializes the return value. `project_path` identifies the selected
execution workspace and may be sparse/ephemeral; writing there does not establish
durable publication or authority to modify the live project.

An external convenience client could eventually look like this. Names and
signatures are proposals, not an existing SDK:

```typescript
const run = await client.execute({
  itemRef: "graph:example/process-readings",
  parameters: { source: selectedSource },
  projectPath: selectedProject,
});

for await (const event of run.events()) {
  updateView(event);
}
const outcome = await run.result();
```

The definition and its supported bindings select the environment/content; the
client does not invent provider, runtime or filesystem authority. A desktop
client, backend and browser may expose similar operations while using their
distinct authentication mechanisms. Browser code must not receive a privileged
operator/node private key to make the example work.

Signed HTTP clients are an existing documented baseline. RyeOS UI also has
browser-session and seat-binding mechanisms; those are scoped UI authority, not
permission to call every signed HTTP endpoint. A general browser SDK must map
its selected operations to the actual admitted binding/route contract. Extending
that surface remains separate work; a cookie cannot substitute for signing or
delegated execution authority.

Where a selected worker explicitly supports lifecycle commands, its client can
expose command/status/terminate and supported checkpoint operations. Their result
types must distinguish acknowledged request, completed work, physical process
closure, unresolved external resources and publication/evaluation status.

Provider adapters can be written in another language when the admitted launch
and host-operation contract supports it. The adapter interprets provider requests
and responses; RyeOS retains grants, intent, original operation/deadline and
recovery ownership. A semantic checker interprets selected observations; it
cannot attest process death or settlement owned elsewhere.

## SDK and host boundary

Client libraries should own encoding, authenticated transport, supported stream
decoding and typed errors. Admission, execution identity, authoritative state and
policy decisions remain in their existing RyeOS owners. Avoid reproducing
canonical security validators independently in each SDK.

Pin the selected wire/contract versions and require explicit incompatible-version
refusal. Conformance vectors must cover duplicate keys, exact numeric ranges,
Unicode, framing and bounds, and canonical bytes wherever identities are computed.
JavaScript `number` cannot represent every Rust `u64`; expose an exact contract-
compatible representation without silently changing the wire schema. Default
Python/JS JSON serialization is not automatically RyeOS canonical encoding.
SDKs must not recreate protected signed descriptors or native authority objects.
The companion adapter note owns the detailed cross-language conformance boundary.

An SDK inside a workload must use its selected callback or workload-client
surface. Wrapping arbitrary `subprocess`, filesystem, network or device calls
does not convert them into RyeOS-owned operations. Provider SDKs that create
threads/helpers or hide retries require a compatible execution arrangement.
Unavailable required support refuses explicitly.

The current hosted workload ingress is a finite admitted child-item/call surface
under node, root, project and caller ceilings. Its restricted CLI exposes only
`ryeos execute`, with no general daemon fallback, remote/lifecycle management,
installation or publication interface. Python/TS conveniences over that ingress
must preserve its limits; external client operations cannot all be exposed inside
a worker simply by reusing a client library.

Expose meaningful identity/provenance without making authors manually assemble
protected descriptors or native owner objects. Python/JS garbage collection is
not deterministic resource settlement. Managed resources need explicit lifecycle
operations and native/durable ownership; client disappearance cannot be the sole
cleanup mechanism.

## Results, uncertainty and retention

Define result envelopes separately from immutable artifacts, writable workspace
changes, provider effects and accepted claims. Use existing publication paths;
do not advertise a generic artifact API until its authority and retention
contract exists. Streaming output is not automatically a durable artifact.

Reconnection observes the original operation through authorized handles. Lost
submission acknowledgements need an explicit discovery/idempotency contract;
do not promise exactly-once effects or resubmit automatically. Cancellation
request, cancellation acknowledgement and completed cleanup are separate states.
Retries preserve original bounds and effect coordinates where the contract
requires them.

Retained code/runtime/checker content and reference discovery must remain
available for unresolved operations and supported historical verification.
SDK handles cannot become a second retention authority. Service removal and
schema evolution follow native composition/state owners; keep required atomic
publication, canonical references and recovery paths.

## Staged implementation and acceptance

1. **Inventory current consumers.** Map exact operations and authentication for
   external clients, direct tools and hosted workers. Record supported versus
   missing surfaces, including artifact publication and cancellation/reconnect.
2. **Expose one existing useful path.** Build a small maintained client over the
   actual execution/service contract, with meaningful authentication, rejection,
   streaming and lost-connection behavior. Select Python or TypeScript based on
   a real consumer. No broad SDK catalogue is required first.
3. **Pair authoring with consumption.** Supply an admitted interpreted tool or
   worker plus reusable environment and local authoring harness. Exercise the
   actual installed host; synthetic protocol success is insufficient.
4. **Prove the current adapter boundary.** Complete the selected integration
   and original ownership/admission/qualification gates. Make two meaningful
   successive interpreted changes with unchanged complete native binaries.
5. **Prove reuse, then extend through concrete demand.** Exercise a second
   materially different consumer, including ordinary application composition
   beyond provider integration, before claiming a reusable application path.
   Add another execution form, explicit
   remote placement or content-publication interface when its owner contract
   supports the selected consumer. Generalize after showing where contracts
   agree; retain capability differences.

Do not reset the active implementation sequence or insert this entire programme
before its current usable workflows. The first client can be scoped independently
where existing contracts suffice; deeper authority/runtime changes remain
coordinated with their current owners.

The authoring harness should offer fast local source/schema and proportionate
behavioral feedback separately from installed admitted execution. Label what each
lane establishes. Reuse unchanged interpreter/dependency closures; ordinary
product edits should not trigger unrelated native builds or full release
qualification. Measure local feedback and admitted-run costs separately.

Acceptance distinguishes source review, builds, installed admission, actual
execution and operational qualification. Measure edit-to-local-feedback and
edit-to-admitted-run costs, including packaging, transpilation and signing.
Each slice proves its applicable claims: unsupported-capability refusal,
original identity and limits, and the cancellation/recovery/output behavior it
actually supports. Report unavailable operations explicitly. The two successive
unchanged-native changes are acceptance for the adapter slice, not a prerequisite
for exposing an already supported external-client operation. A smaller dependency graph or
successful JSON exchange does not establish these claims.

## Ownership and related documents

This note owns the cross-language application authoring/control experience and
staged SDK questions. Existing documents retain their mechanism ownership:

- [Environment build system](environment-build-system.md): production versus
  consumption, reusable environment build operations and publication.
- [Native development platform](ryeos-native-development-platform.md) and
  [native bundle publication and node composition](native-bundle-publication-and-node-composition.md):
  exact project generations, development checks, release publication and
  activation separation. Clients and authoring harnesses may invoke authorized
  operations; running checks does not confer publication or deployment authority.
- [Tool runtime authority](tool-runtime-authority.md) and its
  [advanced path](tool-runtime-authority-advanced-path.md): callback authority
  and future delegation/nested invocation.
- [Managed runtime workers](content-addressed-managed-runtime-workers.md):
  future leased invocations, eligibility and reset semantics.
- [Substrate growth roadmap](substrate-growth-roadmap.md),
  [distributed substrate](distributed-substrate-deferred-advanced.md) and
  [local execution roadmap](local-execution-roadmap.md): execution growth,
  placement/federation and inference/training sequencing.
- [Large-content follow-ons](large-content-realization-follow-ons.md): wider
  artifact/content storage and composition work.
- [Python runtime contract](../../../../bundles/standard/.ai/knowledge/ryeos/core/runtimes/python-runtime-contract.md),
  [HTTP API](../../../../bundles/standard/.ai/knowledge/ryeos/core/node/http-api.md),
  [worker-hosted execution](../../../../bundles/standard/.ai/knowledge/ryeos/core/execution/worker-hosted-execution.md)
  and [execution isolation](../../../../bundles/standard/.ai/knowledge/ryeos/core/node/execution-isolation.md):
  existing operating surfaces; source documentation is not current qualification.

The companion [Language-Neutral Integration Adapters](language-neutral-integration-adapters.md)
and [Native Modularity and Bundle Composition](native-modularity-and-bundle-composition.md)
own interpreted product launch/trust and native dependency/service/state
composition. They have durable locations in this directory; this note does not
replace their owners or claim their full implementation.
