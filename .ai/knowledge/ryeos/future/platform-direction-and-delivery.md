<!-- ryeos:signed:2026-10-06T01:00:58Z:6b6cc5e2c88aad289257dfb7ede691988737a2b23a1097768bb043a581ee5f51:Yc8PDflQSQRTtBD9e8CxXXFOe+R0tn3t7lsvhe+YDYX7HHagV6R/CUTL+vfGBSCH2EVIiiOZkEL7vIrRRlWBDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: platform-direction-and-delivery
title: RyeOS Platform Direction and Delivery
description: Overall application-platform direction, responsibility boundaries and observable delivery milestones for the future-design set
entry_type: design
version: "0.1.0"
status: direction
```

# RyeOS platform direction and delivery

## Purpose and authority

RyeOS is being developed as a platform for building, running and evolving
applications that combine code, models, tools and services. Applications express
their behavior through admitted implementations and declarations. The shared
substrate supplies execution, authority, resource ownership, durable state and
evidence under the selected execution contract.

This is the overarching project direction agreed in the 2026-10-06 discussion.
It owns the product destination, responsibility boundaries and delivery
milestones for the [future-design set](README.md). It is a target, not a claim
that the full platform is implemented or operationally qualified. This document
does not allocate implementation work, authorize provider contact or alter an
active campaign's acceptance procedure. Detailed mechanism owners retain their
contracts; current operating knowledge and actual qualified implementations
determine what a user can do today.

## The experience people should be able to use

An author should be able to develop in a supported language, compose existing
capabilities, select runtime environments, add views, operate the application
and distribute new generations without rebuilding unrelated native components.
An operator should be able to see what is running, what it can access, what
completed, what needs attention and what remains owned after interruption.
A person using the application should interact through understandable controls,
results and decisions, with implementation evidence available when it explains
an outcome.

The common journey is:

```text
author code and declarations
  -> exercise locally
  -> select exact source, runtime and authority for admitted execution
  -> compose capabilities and add an interface
  -> observe, cancel or recover under the supported lifecycle
  -> retain and reuse permitted results
  -> publish and select a successor generation
```

Local development feedback, admitted execution, qualification, publication and
activation establish different facts. Each needs a usable interface. A local
fixture does not establish installed acceptance; an ordinary source edit should
reuse unchanged runtimes and avoid unrelated full-platform builds.

Applications may include tools, managed workers, graphs, directives, providers,
semantic checkers, configuration and views. Their exact code and dependency
closures can be referenced through existing content and bundle mechanisms.
"Application" describes their coherent user-facing composition; it does not
introduce a new executable kind, global application manager or parallel store.
Not every application needs every mechanism, a model, remote compute or a new
UI. An existing CLI or view can provide its first usable interface.

There are two supported directions of integration: author capabilities hosted
by RyeOS, and drive permitted RyeOS operations from an external application.
Hosted callbacks, signed external clients and browser sessions retain their
distinct authority. Similar client ergonomics must preserve those differences.

## Responsibility and change boundaries

| Responsibility | Expected owner | Ordinary change |
| --- | --- | --- |
| Parameters, supported mappings, policy selection and workflow composition | Signed declarations interpreted through existing contracts | New admitted data generation |
| Product protocols, application algorithms, provider behavior and semantic interpretation | Selected implementation package in a supported language | New code/source generation and affected checks |
| Views and client conveniences | Admitted view/client implementation using supported operations | Presentation/client generation with accurate capability handling |
| Exact admission, grants, process/resource custody, canonical references and durable transitions | Shared native substrate and explicitly selected native services | Native release when the shared contract or mechanism changes |
| Build, qualification, publication and activation | Separately authorized operations and existing release owners | Exact products and explicit decisions at each transition |

Use an existing declaration when it expresses the behavior. Use admitted
procedural code when the behavior needs code. Add native machinery when a
concrete requirement exposes a missing shared mechanism or guarantee. Rust
changes remain appropriate for those requirements; the platform is not frozen.

Language does not decide trust. An independently selected Python checker may
own product interpretation. The substrate owns its admission and the joining of
that interpretation to facts observed by other owners. Repeating every vendor
parser in Rust would preserve the application rebuild dependency. Conversely,
moving canonical references or resource obligations into opaque application
JSON would prevent shared retention, authorization and recovery from working.
Extension contracts must expose the structure those mechanisms require.

State decomposition must preserve reference discovery, atomic publication,
historical interpretation and executable recovery for unresolved work. Optional
installation, activation, compilation and safe removal are separate properties.
An interpreted implementation still depends on the native capabilities required
by its selected contract. Finite supported compositions need explicit coverage.

## Why the parts form one platform

Linked admitted program, invocation and content identities and references should
connect source, runtime dependencies, execution, retained results and later
disposition. Work started by another operation must retain the appropriate parent constraints and
its own resource owner. A client or view observes that work through its actual
authority; it does not invent another lifecycle or source of truth.

Environment production makes exact reusable dependencies. Consumer binding
decides which operation may use them. Isolation governs actual access and
containment. Placement selects a supported site with target admission. None of
these facts substitutes for another. Remote execution and checkpoints promise
only the portability envelope their qualified contract supports.

Reusable results retain their provenance and permitted uses. Observations,
semantic judgments, accepted claims and future authority remain distinct.
Signatures establish attribution/integrity under their trust assumptions; they
do not establish usefulness, external truth or learning. Models supply selected
judgment within an application and cannot award themselves new authority.

The product hypothesis is that preserving these connections reduces the work
each application must do to become operable, inspectable and reusable. Validate
that hypothesis through author and operator tasks and measured iteration costs.
The number of mechanisms or signed records is not a measure of user value.

## Delivery milestones

These are observable platform outcomes, not a new serial implementation queue.
Useful clients can ship against existing contracts. Missing shared guarantees
must be completed and qualified before applications rely on them. Keep the
active Render/Codex and Modal ownership and qualification requirements intact;
use their selected requirements to prove reusable boundaries where appropriate.

### 1. One complete application development path

Select one real consumer, one implementation language and a finite execution
arrangement. Python has an existing tool-runtime basis; select the actual path
from the consumer's needs. A TypeScript control client and an admitted
TypeScript runtime are separate deliverables.

Demonstrate that an author can:

- implement a useful capability outside Rust with explicit inputs and outputs;
- bind admitted source and reusable interpreter/dependency content;
- invoke it through an authenticated client or permitted hosted surface;
- compose it with another capability using existing mechanisms;
- operate it through a usable CLI or view, with progress/results and meaningful
  refusals, and its declared cancellation/recovery behavior;
- retain and inspect outputs through supported result/artifact retention paths,
  using separate publication authority where the selected operation requires it; and
- make two meaningful successive implementation changes that admit and execute
  with unchanged complete native artifacts required by the selected path.

Record exact artifact identities before and after, affected qualification,
environment reuse, packaging/signing costs, edit-to-local-feedback and
edit-to-admitted-run time. A successful JSON exchange or reduced Cargo graph
alone does not pass. A fully general artifact callback, warm invocation pool,
new browser SDK and all-language runtime support are not prerequisites. Name
unsupported operations and demonstrate honest refusal where applicable.

This milestone includes selecting new admitted application revisions through
an existing supported path. A broad bundle catalog/distribution service belongs
to the independent-publication milestone below.

### 2. Reuse across materially different applications

Use the same authoring, admission and client contracts for a second real
consumer with different domain semantics. At least one proving application
should exercise ordinary application composition beyond provider integration.
Reuse mechanism owners; keep capability differences explicit. Demonstrate that
new domain behavior does not require shared native code to know its product
name, fields or protocol semantics. A newly required shared capability can
justify a native extension, but does not itself prove unchanged-native reuse.

Measure author work, debugging effort and native changes as well as execution
correctness. Repeated requirements justify extracting shared operations; avoid
speculative universality before that evidence exists.

### 3. Independent application publication and operation

Qualify the selected path from an exact candidate through checks, publisher
authorization, consumer admission and activation. Show that an application-only
release reuses the substrate image and native binaries. Preserve existing
Core keep-only, state-migration and stopped/drained activation restrictions
until their owner separately qualifies a successor contract. New generations
do not silently replace the identity of already-running or recoverable work.

Publication work can proceed in parallel under its own owner. Early authoring
does not require full native source hosting, replacement of external CI or
automatic deployment. Their useful operations can become platform applications
as their contracts mature.

### 4. Broader execution and retained usefulness

Extend through actual requirements: another language/runtime, worker lifecycle,
specialized compute, explicit site, offline export, recurring work, reusable
knowledge or cooperation among principals. Each extension states what it adds
to an application's experience and qualifies its own authority and failure
behavior. Distributed scheduling, hostile tenancy, numerical exactness and
adaptive learning keep their separate evidence gates.

Research, development campaigns and creative applications are consumers and
proving grounds. Their domain-specific requirements do not automatically become
prerequisites for every application or new variants in shared state.

## Ownership across the future documents

| Owner | Role in this direction |
| --- | --- |
| [Application development and execution](language-neutral-application-development-and-execution.md) | Cross-language authoring/control surfaces, capability accuracy and SDK staging |
| [Integration adapters](language-neutral-integration-adapters.md) | Interpreted product launch, protocol/checker interpretation and trust boundaries |
| [Native modularity](native-modularity-and-bundle-composition.md) | Dependency reduction and optional service/state composition |
| [Environment build system](environment-build-system.md) | Production, verification and simple consumption of runtime closures |
| [Native publication](native-bundle-publication-and-node-composition.md) | Independent generations, catalog/consumer composition and activation contracts |
| [Nested execution](nested-execution-ownership-and-evidence.md) and [tool authority](tool-runtime-authority.md) | Child/resource ownership, fact ownership and applicable runtime grants |
| [Substrate growth](substrate-growth-roadmap.md) | Execution branch dependencies and triggers beneath this platform direction |
| [Native development platform](ryeos-native-development-platform.md) | Development/release applications built on the platform, including later source hosting |
| [Enduring working environment](enduring-working-environment.md) | Principal continuity, research interpretation and longer-term product questions |

The [index](README.md) maps every future note into this direction, identifies
its maturity and states its trigger/proof. A branch sequence applies within its
declared branch; it does not independently set project-wide priority. Operating
contracts remain with their existing core/bundle owners. Branch checkpoints and
implementation receipts remain the evidence for current campaign status.

## How to choose and maintain the next slice

Before allocating work, name the application requirement or shared guarantee,
the actual consumer, existing owner, missing contract, bounded acceptance and
effect on the development loop. Classify whether data, application code or
native machinery must change. Include required compatibility, recovery and
retention consequences; avoid incidental broad refactors.

Architecture importance and scheduling priority are separate. This direction
justifies making a usable application development path a first-class delivery
target; it does not justify implementing every future mechanism first. Prefer
a complete usable path over accumulating unconnected infrastructure. Preserve
necessary guarantees and make their author-facing interfaces understandable.

When a slice lands, update the relevant operating contract and remove or narrow
its future proposal. Record source review, build evidence, installed execution
and operational qualification separately. Update the index's maturity summary
when the selection or evidence changes. Review proposed changes for both
mechanism correctness and completion of the author/operator journey, applying
findings to the owning documents rather than leaving a competing review plan.
