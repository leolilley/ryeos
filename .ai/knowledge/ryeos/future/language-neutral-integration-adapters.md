<!-- ryeos:signed:2026-10-06T01:00:58Z:64af03e9d106e53bcff2442a4a1c8452d9920b0c646dfdcc029242e0387fbd61:5u+CfDbcaYPkNsdvBVMiINp1/e55MYeGZodCZtcTkz5kvvRXCvD7Hrs0nI9DrIPcz7p2RNhaV8LQ67OK+pSGBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: language-neutral-integration-adapters
title: Language-Neutral Integration Adapters
description: Develop product integrations and semantic checkers in admitted data, Python, or TypeScript while preserving RyeOS execution and evidence ownership
entry_type: design
version: "0.4.1"
status: discussion
```

# Language-Neutral Integration Adapters

This note owns interpreted product launch and trust boundaries beneath the
[platform direction](platform-direction-and-delivery.md). The broader author
and client journey belongs to [application development and execution](language-neutral-application-development-and-execution.md).
Restored from the preserved 2026-10-05 worktree on 2026-10-06; dated observations
remain historical evidence, not current qualification.

## Direction and scope

Routine integration work should be possible without compiling the RyeOS Rust
workspace. Adding a worker product, provider API mapping, runtime-specific
checker, or compatible inference endpoint should change its signed definitions
and selected implementation package. Rust changes remain appropriate when
RyeOS needs a genuinely new host capability, execution guarantee, or shared
protocol operation.

This is a future direction, not an implemented SDK or a change to current
Render, Codex, Modal or qualification acceptance. The first usable interpreted
application path is now a platform delivery target; selecting its concrete
implementation slice remains coordinated with the active owners.
The desired improvement is fewer Rust rebuilds in ordinary development and a
smaller amount of product-specific Rust to maintain as integrations grow.

The crucial boundary is **authority and evidence ownership**, rather than
programming language. A selected Python verifier can be trusted to interpret a
product protocol. Rust must not need to repeat that interpretation for every
new application. RyeOS retains control of which verifier is accepted, what
inputs it received, which facts it may attest, and how its findings combine
with independently owned execution facts.

## Existing foundations: reuse before extension

These are source observations, not claims of installed operational acceptance.
Active implementation worktrees may be ahead of the main checkout. Reinspect
the selected implementation before changing its boundary.

| Existing surface | What it already supplies | Consequence for this direction |
| --- | --- | --- |
| Python and Bash Tool runtimes | Admitted source execution, parameters, selected workspace, runtime selection and result handling | New ordinary tools already need no product-specific Rust |
| Structured-session bridge | Shared hosted-worker transport and signed provider profiles; Codex and OpenCode both select it | A new compatible application may need only bundle data; do not introduce another worker driver automatically |
| Model provider configuration | Existing remote HTTP and admitted local worker contracts | A compatible endpoint or local model worker should first reuse those contracts |
| Generic product qualification | Selected signed verifier Tools and scoped claims; GNU Python qualification already uses interpreted code | Verification semantics are not inherently Rust-owned |
| Handler and callback contracts | JSON protocols for parsers, launch preparation, evidence projection and controlled interaction | Reuse the appropriate existing protocol rather than inventing a universal adapter channel |
| External lifecycle boundary | Versioned JSON requests/responses, protected descriptors, signed adapter declarations and durable operation owners | The wire is already usable across languages; interpreted launch and dependency admission are the concrete gaps |

Relevant sources:

- [Python runtime contract](../../../../bundles/standard/.ai/knowledge/ryeos/core/runtimes/python-runtime-contract.md)
  and [Python function runtime](../../../../bundles/core/.ai/tools/ryeos/core/runtimes/python/function.yaml).
- [OpenCode bundle](../../../../bundles/opencode/README.md) and its
  [hosted worker](../../../../bundles/opencode/.ai/workers/opencode/hosted.yaml).
- [Provider contract](../../../../crates/engine/ryeos-provider-contract/src/lib.rs)
  and [directive provider adapters](../../../../crates/runtimes/directive/src/provider_adapter/mod.rs).
- [GNU Python verifier](../../../../bundles/standard/.ai/tools/ryeos/environments/qualification/gnu-python.yaml)
  and [handler protocol](../../../../crates/engine/ryeos-handler-protocol/src/lib.rs).
- [External lifecycle wire and declarations](../../../../crates/engine/ryeos-external-execution-contract/src/lib.rs),
  [invocation](../../../../crates/engine/ryeos-external-execution/src/lifecycle_adapter.rs),
  [daemon operation adapter](../../../../crates/daemon/ryeos-app/src/external_lifecycle_adapter.rs),
  and [retained artifacts](../../../../crates/daemon/ryeos-app/src/external_artifacts.rs).

The lifecycle boundary is currently oriented around a selected native
executable, captured through a signed `bin:` reference and invoked through a
retained executable descriptor with fixed operation arguments. It does not
declare a separate interpreter, script/module closure, or language dependencies.
This is a launch and artifact-model limitation; JSON itself is not Rust-only.
A compiled implementation in another language does not inherently need a new
wire protocol, while an interpreted implementation needs its entire launch
closure explicitly admitted.

Do not infer full sandboxing from this process boundary. The current private
lifecycle runner explicitly has no supervised attachment or process scope.
Selected snapshot operations forbid process creation; the Linux filter also
blocks thread creation through clone/clone3. An interpreter or SDK that starts
threads or helpers cannot simply replace the executable under that profile.
Supporting it requires a reviewed execution arrangement with equivalent stated
guarantees, not silently removing the restriction.

## Active implementation trajectory

The 2026-10-03 worktree review found an existing partial implementation of this
direction. The commits below are historical source snapshots, not current
delivery status. The original coordination records under
`.tmp/arc-render-october-20261001/` describe that historical campaign. The later
modularity continuation has its own active checkpoints; consult the selected
campaign's latest records for operational status. Later build or component-check
success does not establish admitted execution.
Qualification rehearsal `1f8eafffd750` and Render/Modal integration
`d8753fb32981` already separate Codex interpretation into the selected
`codex-runtime-verifier` package. Its semantic checks explicitly do not establish
native death, provider settlement, writer exclusion or qualification issuance.
Signed profiles select finite scenarios and fixtures; new protocol semantics
still require changing compiled product code. This is a useful ownership seam
for an interpreted checker, not completed language-neutral launch support.

Those trees also expose a smaller procedural seam: qualification request
binding. A shared bounded contract carries public template data and returns a
resolved body bound to the input digest. The selected Codex implementation owns
the template grammar; the daemon retains adapter identity and input/output
bindings. This mapping confers no contact or qualification authority. Its
current invocation still selects a native executable. An admitted interpreted
implementation could exercise launch/source/runtime closure support without
first moving credential-bearing provider lifecycle logic. It would establish
only that mapping capability, not full interpreted lifecycle support.

Render's signed provider specification supplies finite variation inside a
closed Render implementation. Provider API behavior and lifecycle invocation
remain Rust/native-executable work in these trees. Supporting another provider's
procedural behavior in Python or TypeScript remains a concrete extension gap.

Current Modal/GNU-CUDA work already places loader validation, NVML interpretation,
CUDA lifecycle tracking and product qualification in Python. Native work retains
actual process/channel/resource custody and joins observations to the original
admitted operation. Startup admission `bc24dd5c6c20` nevertheless closes some
first-product choices in Rust: the `tinygrad` realization, `python-gnu` layout,
Python 3.14 members, NVML library member and GNU-specific launch coordinates.
A new interpreter layout or backend can therefore still require Rust edits.

The future extraction should separate the selected product recipe and its
interpretation from common checks that bind actual admitted artifacts, launch,
resources, original occurrence and deadline. Preserve those native ownership
checks; an adapter's assertion cannot replace them. A second concrete startup
contract should establish which variation needs generalizing.

These observations support continuing the existing ownership work while
preparing narrow language-neutral seams. Source preparation need not wait for
all current operational qualification, but source presence does not establish
installed acceptance. The active NVML and driver worktrees were changing during
review; their Python work is provisional source evidence. No build, provider
contact, GPU run or runtime acceptance was performed for this review.

## Three ways to extend RyeOS

Choose the least additional machinery that expresses the concrete integration:

1. **Signed data:** existing finite profiles express routes, field mappings,
   message schemas, runtime selection, models and operation choices.
2. **Admitted implementation code:** Python, TypeScript or another language
   handles procedural product behavior that those profiles cannot express.
   This includes semantic checking when a selected verifier owns that role.
3. **Shared native capability:** Rust implements a missing host mechanism or
   generic execution/evidence contract. It should benefit the concrete
   consumers that justify it and remain free of provider-name branches.

These are complementary. Do not grow a YAML expression language to hide an
entire program, or add a scripted adapter when existing signed data suffices.
The implementation language of an extension is distinct from its authority.

## Responsibility and trust

The following are roles, not proposals for new RyeOS kinds, registries or
parallel execution systems. Bind them to existing Tools, Workers, Protocols,
Handlers, Configs, products and lifecycle declarations where those owners fit.

| Role | Owns | May not substitute for |
| --- | --- | --- |
| RyeOS admission and operation owners | Grants, selected identities, durable intent, invocation bounds, recovery decisions and policy acceptance | Product protocol interpretation it has not independently implemented |
| Lillux host owner | Actual process/descriptor/socket mechanisms, interruption, waiting and settlement observations under the selected contract | Application semantics or provider truth |
| Product implementation | Vendor requests, configuration, event translation and procedural interaction within admitted authority | Its own grants or evidence that another owner must supply |
| Selected semantic checker | Product-specific framing, correlation, protocol validity and bounded scenario verdict | Daemon launch, containment, settlement or candidate freeze facts |
| Qualification policy and owner | Required evidence owners, selected checker and scenario, identity joins and acceptance of a scoped claim | Arbitrary candidate-authored success |
| Candidate/task code | Proposed outputs and explicitly labelled self-reports | Its own evaluator selection, acceptance or publication |

A semantic checker is trusted executable code for a declared interpretation,
whether written in Rust, Python or TypeScript. Signing establishes its source
and integrity; it does not establish that the checker is correct. Review,
conformance cases and qualification justify the selected trust.

Independence means the subject cannot choose or replace its own checker,
scenario, expected inputs or required policy. The checker receives the exact
selected observations, preferably without credentials or mutable subject state
when interpreting a transcript. If its implementation is also part of what is
being tested, qualification needs a different admitted observer for that claim.
An adapter can report normal operation completion, but its self-report cannot
satisfy a requirement for evidence from a separate owner.

For example, a Python checker may conclude that a new agent's transcript
contains one correlated completed turn with the expected tool exchange. RyeOS
checks who selected that checker, its source/runtime identity, transcript and
attempt binding, and the required process evidence. RyeOS does not parse that
agent's event vocabulary a second time in Rust. A successful checker process
and a passing semantic check are separate facts; both can still coexist with
missing settlement evidence and therefore fail the overall qualification.

## Roles of product integrations

A new integration should inventory the following pieces and reuse the existing
implementation for each piece that already fits.

- **Hosted application interaction:** first try the structured-session bridge
  and signed profiles. If a genuinely different interactive protocol needs
  code, place its semantics in a selected implementation with existing
  managed interaction and process ownership. Do not create a feature-owned
  supervisor whose report becomes daemon evidence.
- **Provider lifecycle:** vendor API behavior, response parsing and
  reconciliation mapping belong to the provider implementation. RyeOS owns
  allocation intent, allowed operations and durable uncertainty.
- **Inference:** reuse provider transport or the admitted local worker route.
  Endpoint formatting, tokenization, model logic and product-specific numeric
  checks need not be native core code. GPU process and device custody remain
  with their actual owners and selected evidence requirements.
- **Verification:** semantic parsers and scenario checks can be interpreted
  Tools or handlers when the existing contract fits. Evidence transport and
  generic policy joins should remain reusable across products.
- **Runtime production:** source assembly and packaging can use ordinary
  admitted Tools and Graphs. Product identity, qualification, publication and
  installation keep their current distinct owners.

The long-term unit a contributor develops is therefore an optional integration
bundle: definitions plus any implementation/checker source, schemas, fixtures
and exact runtime dependencies. It should not imply one new executable, one new
SDK, or a new daemon branch for every product.

## Stable envelopes and extensible product semantics

Reuse existing wire versions and channels before adding another protocol.
Document/export their schemas and provide conformance fixtures and language
helpers where a real author needs them. The current lifecycle JSON/descriptor
transport, handler stdin/stdout transport and managed interactive callbacks
have different authority and lifetime rules; combining them into one channel
is not a prerequisite.

Separate validation ownership:

- RyeOS validates the closed common envelope: operation/attempt, selected
  implementation, role, bounds, capability, evidence references and the
  applicable contract version.
- The selected schema/interpreter validates its versioned product payload,
  event vocabulary, correlation rules and semantic findings.
- Unknown platform authority or unsupported protocol versions refuse.
  Product-specific payloads must be bounded and validated by their admitted
  owner; they must not require a Rust enum variant for each vendor event.

Cross-language conformance includes duplicate-key handling, exact integer
ranges, Unicode, framing, depth/byte bounds and canonical bytes wherever
digests are computed. JavaScript numbers must not round Rust u64 coordinates.
Do not replace RyeOS's canonical encoding with default Python/JS JSON output
or assume a valid schema proves semantic correctness. Reuse the canonical
byte owner where possible and ship exact vectors for other implementations.

SDK validation is convenience. The authoritative receiver enforces the
contract. A language helper may expose already granted operations; it cannot
create capabilities or turn a path supplied by the extension into authority.

Version compatibility is explicit. A stable boundary need not accept every
historical version or ignore unknown security-relevant fields. A core contract
migration can require coordinated artifacts. A new product payload supported
by the same common envelope should only change that product's selected package.

## Host effects, credentials and evidence provenance

A separate process and a signed package do not make arbitrary Python/Node code
unprivileged. Audit SDK behavior: networking, DNS, trust stores, retries,
subprocesses, timers, imports, filesystem access and ambient configuration.
Follow [Host Capability Ownership and Review](../development/host-capability-boundary.md)
and [Dependency Constitution](../development/dependency-constitution.md).

The current lifecycle implementation transfers a credential through a sealed
descriptor to the trusted adapter. It does not already provide an opaque
credential broker. Future implementations must state which restrictions are
actually enforced and which depend on trusted product code.

Possible mechanisms include provider-scoped credentials, fixed admitted
request projections, a separately selected request-policy checker, or a
host-owned bounded transport exposed through an explicit capability. Choose a
mechanism for a concrete operation before generalizing it.

A generic broker can constrain destinations, credential use, byte budgets,
deadlines and request count. It cannot know that a vendor JSON field selects a
larger GPU without a provider-specific interpretation somewhere. If a selected
Python request checker owns that interpretation, it is trusted for it; Rust
must not duplicate every vendor parser. If the adapter owns a raw credential,
it is a privileged trusted integration with the authority that credential
really permits. Merely replacing the token with an opaque handle does not
constrain the effects that handle can request.

A claim that all contact is mediated requires actual prevention of bypass.
An SDK convention is insufficient. Existing Lillux/shared transport ownership
must survive the change; ordinary Python networking libraries do not
automatically satisfy that host contract. A narrow native host service or
binding may be needed and could require an initial Rust change. Its interface
must describe reusable host operations, not provider-specific APIs.

Retained evidence records its source and limits:

| Observation | What it establishes |
| --- | --- |
| Daemon/Lillux execution record | The launch, capture or settlement facts actually observed under its contract |
| Transport-owned response capture | Bytes received on the admitted contact, with destination and operation binding |
| Trusted checker result | The stated interpretation of its selected inputs |
| Provider statement | What that provider asserts; it is not automatically proof of guest contents or containment |
| Adapter/subject-supplied transcript | Bytes that component supplied; a digest alone cannot establish where they came from |

Identity, confinement, semantic correctness and independence are different
properties. Preserving a response hash proves retention fidelity, not that a
provider sent it. Provider termination does not by itself establish writer
exclusion or settled guest execution. Required owners depend on the selected
claim; do not impose the strongest Codex guarantee on every trusted point
measurement or quietly weaken the strong claim.

Evidence may include secrets or private project data. Bound and retain it
through the existing access and disclosure policy. Record necessary digests
and provenance without placing provider tokens in public logs or receipts.

## Runtime closure and bootstrap

Reuse existing signed source ownership, runtime realizations, retained
products, bundle publication and qualification. Extend the lifecycle launch
selection only for the missing interpreter/source/dependency relationship;
do not start another package registry or dependency resolver.

The admitted closure must account for:

- exact source, entrypoint, interpreter or generated executable;
- resolved dependency bytes, assets, helper programs and native extensions;
- target OS/architecture/ABI, dynamic loader and libraries;
- explicit platform prerequisites such as kernel or GPU driver/device
  requirements, qualified separately where appropriate;
- module/import/search paths, environment and read/write locations; and
- protocol, semantic checker and scenario identities required by its role.

A dependency lock helps build that closure but does not replace its retained
bytes. A signed source file does not pin what a mutable package directory
imports. TypeScript needs an explicit build/runtime choice; Node is one
possible runtime rather than an inherent requirement of the language.
Generated JavaScript and its source/build provenance must stay bound.

A controller-side host adapter's interpreter must be available before that
adapter creates a guest. The guest cannot supply its own creator's bootstrap
runtime. Keep host adapter, guest workload and verifier runtimes distinct and
avoid circular self-qualification. Retain the needed closure for in-flight
operations and recovery when a newer bundle is installed.

Runtime compatibility is an admission concern. Node, Python dependencies or
provider SDKs may start threads/helpers or need native libraries. Qualify the
actual runtime under the selected invocation contract. No-process-creation
profiles cannot simply enable threads to make an SDK start.

## Failure and recovery

Use existing allocation, activation and termination owners. Language changes
must preserve their guarantees through partial startup and lost responses:

1. Record operation intent and the exact implementation/runtime selection
   before provider contact.
2. Bind provider idempotency keys or reservation identity where supported.
   Return or durably hand off provider locators through the owning operation.
3. A crash after provider acceptance leaves an uncertain operation. It does
   not imply that no resource was created.
4. Recovery reconciles through the admitted protocol and retained identity.
   Blindly replaying create or a model request can duplicate resources or
   charges. If the provider cannot reconcile the effect, retain uncertainty.
5. Local cancellation and process death do not undo remote work. Cleanup can
   need its own admitted authority and deadline after the original deadline.
   A useful late locator can be retained for reconciliation without treating
   the original action as timely success.
6. Preserve the distinction among stream EOF, child exit, whole-scope
   settlement, provider termination and frozen candidate output.

SDK automatic retries, token refresh and background polling must fit this
contract. Adapter upgrades cannot silently replace the implementation used
to interpret or clean up an unresolved historical operation. Any migration
needs an explicit supported compatibility and evidence path.

## The UI precedent and its limit

The UI already has a Rust-owned shared model, a typed Rust/WASM boundary,
generated TypeScript contracts, a Svelte browser renderer and a Rust terminal
renderer. Browser presentation changes can reuse the installed semantic core.
See [UI development](../development/ui-development.md).

The same development benefit is wanted here: reuse a stable installed native
core while changing selected product packages. There is an additional step:
some product semantics, including semantic verification, must also belong
outside the core. Keeping every provider parser in Rust would preserve the
rebuild dependency even with a Python adapter in front.

A renderer's events still undergo daemon authorization. An adapter or checker
may hold explicit authority or be trusted for interpretation. Their boundary
must document those responsibilities rather than infer trust from language
or from the fact that the UI has a similar structural split.

## Authoring loop and development speed

The target loop is:

1. Determine whether existing profiles, Tools, Workers or Handlers already
   express the integration.
2. Edit source/data and exercise the existing protocol in a local harness,
   using fixtures and fault injection as development evidence.
3. Package a fresh exact revision with existing signing and product machinery.
   Development fixtures cannot silently become production qualification.
4. Select that revision for a bounded admitted run. Reuse the installed core
   and its unchanged native artifacts if the common contract has not changed.
5. Review the resulting evidence and qualify/release through existing owners.

Avoid a new SDK build or whole-workspace release becoming mandatory for a
provider mapping or checker edit. Large interpreter/runtime artifacts should
be reusable exact dependencies rather than rebuilt for each source change.
Some dependencies still need native compilation, and TypeScript may need
transpilation; that does not require rebuilding the RyeOS Rust workspace.

Measure edit-to-feedback and edit-to-admitted-run time, Rust builds triggered,
artifact bytes changed, runtime reuse and the actual checks needed. Hashes of
the daemon and shared native runner should remain unchanged for the ordinary
extension-only change. This tests the user's goal directly rather than merely
showing that a Python subprocess can return JSON.

## Staging and the first decisive proof

Preparatory work now: record the ownership boundary, map existing extension
points, identify the first concrete missing composition, and stop adding
provider-specific Rust to shared machinery when an existing supported
extension can express it. This does not freeze necessary current core fixes.

Select a real integration that needs procedural product code, including current
work where it demonstrates the missing boundary, to drive the implementation
slice after its source dependencies and active owners are clear.
If signed profiles already express that integration, use them; such a case does
not justify extending execution machinery. Preserve the current Render, Codex
and Modal delivery work. Their migration is not a prerequisite. Existing
implementations remain references; wholesale migration is unnecessary. A selected
current implementation can establish the boundary when its actual needs justify it.

Choose the first missing capability from that integration's actual needs. It
may be configuration mapping, semantic checking, managed worker interaction or
provider lifecycle execution. The existing qualification request mapper is a
possible small proving case when needed by the integration, not a mandatory
standalone migration project. No checker-first sequence, new scheduler,
standalone inference service or universal plugin framework is required.

The October 5, 2026 Pacific/Auckland checkpoint was a historical planning
boundary. Current prioritization follows the platform milestone and active
continuation's concrete source and qualification gaps. Reuse selected current
integrations where they establish the missing boundary; a future provider is
not required to justify already observed development needs. This does not
require wholesale migration of existing adapters or changing their acceptance.
Completion of every infrastructure ambition is not a prerequisite for a useful
client or an authoring path supported by existing contracts.

The first implementation includes any necessary shared launch/admission changes,
one concrete product implementation, review and qualification of its actual
authority and failure behavior, and a reusable authoring example. Admission must
cover the selected interpreter, entry point, source/dependencies and retained
runtime. Estimate this work after choosing the consumer and protocol: a pure
mapper and a credential-bearing lifecycle adapter have different scopes. No
fixed delivery estimate or support for both Python and TypeScript is promised.

The decisive proof is a real product behavior change, followed by a further
ordinary product change, admitted and executed with unchanged RyeOS Rust
binaries. Existing qualification and release requirements still apply to the
changed package. The proof must name its supported operation and limitations.

For example, if semantic checking is the first missing capability, a useful
proof uses a real second product/scenario whose semantic vocabulary is unknown
to the native core:

- An independently selected Python or TypeScript checker interprets retained
  observations through an existing qualification/handler contract where it fits.
- Changing the product's event shape and checker requires only source/schema
  package changes; the core has no new provider-name branch or semantic parser.
- Changed inputs, checker/runtime substitution and subject-selected policy are
  refused through the applicable admission and evidence rules.
- A passing checker cannot replace missing daemon launch/settlement evidence.
- Development and admitted packaging reuse unchanged core binaries.

An offline checker proves the interpretation and iteration boundary only.
It does not prove interactive worker driving, runtime confinement or provider
lifecycle support. If an existing interpreted verifier already proves this
composition, use it as the baseline and move to the first missing requirement.

Any provider lifecycle proof must cover interpreted launch closure,
credentials/network enforcement, actual runtime compatibility, uncertain
allocation, reconciliation, cancellation, upgrade/recovery, and cleanup. It
must state which guarantees are enforced and which product code is trusted.

Before implementation, resolve these decisions for the chosen slice:

- Which existing kind/protocol owns it, and what exact missing capability
  prevents use today?
- Which facts are core-observed, provider-reported or checker-interpreted?
- Does it need live effects or only pure interpretation?
- What admitted runtime and bootstrap path meet its current host contract?
- How is historical recovery preserved across package changes?
- Which single change and negative case will demonstrate less Rust coupling?

Shared host capability work can require an initial Rust release. The
acceptance target is that subsequent product changes stay outside that release
cycle. Additional languages should follow real consumers, not a promise to
support every runtime at once.

## Relationship to other future work

[Data-Driven Runtime Verification](data-driven-runtime-verification.md)
already assigns product interpretation to an admitted runtime-specific
verifier. This note makes its possible implementation languages and trust
scope explicit.

[Data-Driven Provider Lifecycle Operations](data-driven-provider-lifecycle.md)
proposes finite signed field/route/status mappings. Keep those where they
suffice. Procedural implementations are separately admitted code, not scripts
embedded into that restricted profile vocabulary. That note gates shared profile-interpreter extraction on repeated concrete
need. An admitted procedural implementation is a separate path and retains its
own operational qualification requirements.

[Native Bundle Publication and Node Composition](native-bundle-publication-and-node-composition.md)
and [RyeOS-Native Development Platform](ryeos-native-development-platform.md)
own the broader source/product/release direction. This proposal consumes those
owners rather than introducing a parallel artifact store or deployment system.
[Nested Execution Ownership and Evidence](nested-execution-ownership-and-evidence.md)
governs when the subject's process must be independently owned and observed.
