<!-- ryeos:signed:2026-09-30T04:06:09Z:85f1e5043778f220bbcc6b0d71af430d904b6e673a8cb06c7cc1a39365fad574:2YoFe+ATezXMIN5jW0MHyvH4kSGW8NjvkSC/m7yptGNUY9o8Y6aOrxG25MuQ3pM70CIjy3ZCdN8r0HsVCVuDBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "remote-development-and-qualification"
title: "Remote Development and Qualification Runbook"
description: "Use an operator-controlled stronger host and an ordinary configured RyeOS remote without adding a deployment or scheduling substrate"
entry_type: implementation_guide
version: "1.10.1"
```

# Remote Development and Qualification Runbook

This is the generic workflow for using a stronger machine to build and test
RyeOS and then qualify a workload through RyeOS's existing remote boundary. It
deliberately separates host administration from authenticated RyeOS runtime
operations.

The initiating workstation owns the source commit, review, and final integration.
The stronger target owns its checkout, build cache, disposable qualification
roots, and retained non-secret evidence. Each app root is one ordinary RyeOS
node. A target is selected by the operator as one named configured remote; it
is not discovered by a registry or scheduler.

That ownership statement is the current qualified operating model. The future
[`ryeos-native-development-platform`](../future/ryeos-native-development-platform.md)
and
[`native-bundle-publication-and-node-composition`](../future/native-bundle-publication-and-node-composition.md)
directions move exact source, release execution, retained products, and bundle
publication onto RyeOS-native Graphs and services. Until those paths are
qualified, this runbook must not infer publisher, catalog, installation, or
deployment authority for an ordinary remote build target.

## The boundary

```text
initiating operator                   stronger target operator
  chooses exact commit  ----------->    creates an ordinary checkout
  reviews returned commit/artifacts     builds and runs focused tests
                                         installs/starts a disposable node

initiating RyeOS node                 target RyeOS node
  configured remote + pinned ID  --->   exact node/operator grants
  full-project push/execute/pull        target-local execution and caches
  source-local job transcript      <---   result; signed receipt when supplied
```

RyeOS remote execution is not arbitrary host-shell access. It can execute
admitted signed RyeOS items against an exact pushed project generation, but it
must not be used to install packages, clone RyeOS, invoke unrestricted shell
commands, replace node binaries, or restart the host. Use an existing
operator-controlled transport for that first layer: a pinned CI runner, SSH,
or a cloud provider's console/agent. That transport and its credentials are an
external deployment decision, not a new RyeOS runtime capability.

Do not make the workstation's primary node the experiment. Use fresh source
and target app roots when qualifying lifecycle behavior.

## Existing RyeOS authorities to reuse

No daemon API, node-instance registry, `nodes/` hierarchy, shared filesystem,
scheduler, migration layer, or workload-specific app root is needed.

| Need | Existing authority |
|---|---|
| Select and pin a target | `ryeos remote configure`; stored named remote pins URL, node principal/key, canonical site ID, vault fingerprint, and ingest-ignore policy |
| Bootstrap trust | target-local `admission-token` plus source `remote admit`, or target-local exact `authorize-key`/`authorize-client` |
| Preserve a durable operator principal and origin | explicit configured-operator forwarding for `service:remote/push` and retained-current-HEAD `service:remote/run`, with target `remote_operator` grant and source-node co-signature |
| Transfer a complete project | a `full_project` binding and the typed snapshot/CAS closure used by `remote execute` |
| Execute on target compute | target-local admitted item execution through the configured remote |
| Return project changes | clean-base guarded pull/apply from the result snapshot |
| Install target capabilities | operator-controlled target package/image or stopped-app-root bundle activation from an exact signed artifact |
| Observe recovery | authorized source-local sync job list/inspect and exact remote thread/launch status surfaces |
| Reuse expensive content | host-operator Cargo caches plus target RyeOS CAS, per-snapshot engine, bundle, and managed-realization caches; these are separate authorities |

Ordinary `remote execute` authenticates as the initiating node and is the generic
synchronous full-project push/execute/pull operation. Configured-operator
continuity is a narrower, explicit mode for durable push/run workflows; it is
not selected implicitly and should not be simulated by sharing keys or editing
grants. Choose the mode from the ownership contract of the workload.

There are two supported execution shapes:

- use synchronous `remote execute` for bounded builds, focused tests, and
  model probes whose project results must be pulled back automatically;
- use configured-operator push plus an accepted, retained-current-HEAD
  `remote run` for long jobs where durable target-local thread/result evidence
  is enough.

The second shape returns a validated retained project candidate through the
generic owner-bound `remote worker pull-result` operation. The source does not
supply snapshot hashes: the target reconstructs the current placement, exact
settled route command and completion fence, admitted project/base, candidate
capture, and validation from existing authoritative facts, then signs that
testimony. The source retains it in an idempotent sync job, fetches the exact
closure through ordinary object transport, and uses the existing atomic
clean-base result apply. The target candidate remains retained; pulling never
publishes or discards it. `remote pull` remains the lower-level operation for
caller-known typed object/blob hashes and is not this authority flow.

For a project-owned long worker workflow, use `remote worker run` with a signed
project Config using `ryeos.remote_worker_workflow.v1`. The Config selects one
signed project Graph and maps the task, credential profile, and one explicit
canonical `target_product_selections` batch. The selectors name
destination-local witnesses: they are not source-admitted products, transferred
bytes, or permission to find the latest target binding. The Graph owns provider
selection and its exact worker environment; Core contains no Codex, model, or
project-product branch.

The lifecycle is explicit and finite:

1. `remote worker run` pushes the pinned source generation and establishes the
   target launch. Its intermediate result is `launch_accepted` with
   `allowed_next_action: resume`.
2. `remote worker resume <source_work_id>` performs one recorded recovery or
   target-completion observation. A live target returns
   `completion_pending` and the same `resume` action; it is not recorded as a
   failure. RyeOS does not poll in the background.
3. A completed target Graph must return the exact followed worker terminal as
   `candidate_terminal_thread_id`. The source checks the Graph's effective
   definition digest against the admitted capsule and asks the existing
   `worker-executions/candidate-result` authority for that exact coordinate.
4. Completion records the target-signed candidate testimony and returns
   `allowed_next_action: pull_result`. Pull remains a separate explicit
   `remote worker pull-result` operation.

The first v1 seam accepts only a bounded worker whose terminal thread is also
its candidate chain root. A continued or cross-site-moved child is refused
until a signed lineage resolver exists; a terminal thread ID must never be
silently treated as a general chain-root coordinate. `remote worker status` is
strictly a local read of the source recovery projection and never contacts the
target or advances recovery.

The project Config maps its `task` input directly to the bounded worker's
closed goal envelope. It maps `target_product_selections` only into the
followed worker action; the outer Graph remains product-empty. Ordinary child
admission resolves and seals the exact target-local batch, including a
`content_dependency` selection targeting the worker's `environment` ref
binding and any `workload_execution` selections needed by admitted child
operations. Nothing inherits selections from the Graph parent.

The signed workflow Config uses the current
`ryeos.remote_worker_workflow.v3` schema and declares its exact target runtime
requirements. For example, the RyeOS development workflow requires
`process_control: exclusive_session`, `cleanup_authority:
local_process_scope`, `filesystem_mode: enforce`, and `network_mode: host`.
Before any new project push or launch contact, the source calls the
authenticated `service:node/status` action and verifies those dimensions. The
configured source-operator grant on the destination therefore needs the exact
`ryeos.execute.service.node/status` capability.

The source retains the observed daemon revision, isolation-policy digest,
selected process-readiness reason, and protected process-scope authority
digest in launch acceptance and the final receipt. Query, accepted, and
completion-pending responses expose that evidence. A mismatch refuses before
worker credentials or a provider can be contacted. `pooled_requests` requires
the pool readiness reported by the node but deliberately does not claim an
exclusive-session controller; `exclusive_session` requires the protected
authority digest and a ready controller.

The current start operation uses
`ryeos.remote_worker_workflow_operation.v6`. Predecessor v5 operations remain
readable and recoverable under exactly one retained evidence family. The
pre-cleanup-field family consists of signed v2 workflow Config, v4 progress and
receipt, and v2 launch acceptance; ordinary and pooled work map to
`not_required`, while exclusive work maps to `local_process_scope`. The brief
explicit-local bridge family consists of v3 Config, v5 progress and receipt,
and v3 launch acceptance and requires its authored cleanup field. The retained
job projection selects the family; missing later-phase evidence is allowed,
but mixed-family evidence is contradictory. New v6 operations accept only the
current family. Historical decoding therefore cannot manufacture external
placement authority or silently become new work.

Retained signed session protocol documents authored before the cleanup field
existed are likewise interpreted only after their original signature and
content hash verify. Their historical process mode supplies the same narrow
mapping. Current protocol authoring still requires an explicit field and has
no omission default.

Recovery preserves the ambiguity boundary: once a launch contact may have
occurred, RyeOS first reconciles the exact retained launch ID. An already
accepted launch remains authoritative if the target's current capabilities
later change. Only a proved-absent launch is subjected to a fresh readiness
check before another push or launch. Never relaunch merely because current
readiness differs from the accepted evidence.

Produce the batch on the destination with the existing
`external-content compose-product` authority for each exact consumer and
project generation. Lift the returned `selections` into typed
`content_dependency` or `workload_execution` targets and sort by
target/declaration identity. Do not
type witness or qualification hashes from memory and do not reuse a source-node
composition response. This first seam deliberately has no named profile or
automatic binding lookup.

For the current Codex App Server profile, the bridge injects the workspace
`cwd`, immutable approval policy, and bound `threadId`; the caller must not
supply those fields. Put the complete request in a file so its selection batch
is reviewable and reusable for exact replay:

```yaml
# .tmp/remote-worker-request.yaml
task:
  session_start_payload: {model: gpt-5.3-codex-spark}
  turn_start_payload:
    effort: low
    input:
      - type: text
        text: Make the requested bounded change and use only the admitted verification operations.
target_product_selections:
  # Exact canonical ProductSelectionInput objects derived from this
  # destination's compose-product responses belong here.
  - target: {kind: content_dependency, binding: environment}
    selection:
      declaration_id: authoring-tools
      witness_hash: <destination-compose-product-witness-hash>
      witness_source: {kind: local_capture}
      qualification_hash: <destination-qualification-hash-or-null>
```

The RyeOS development worker also requires exact `workload_execution` entries
for each selected Cargo/format/platform operation; the abbreviated file above
illustrates shape, not a complete development product batch. Launch the
reviewed file through the command-owned input-file path:

```bash
ryeos --project . remote worker run qualification \
  config:development/ryeos/remote-worker personal \
  --input .tmp/remote-worker-request.yaml
```

Retain the returned `source_work_id`. Drive and inspect only through that
stable source-local handle:

```bash
ryeos remote worker status <source_work_id>
ryeos remote worker resume <source_work_id>
```

Repeat the explicit `resume` only when the recorded result says
`allowed_next_action: resume`. Once it says `pull_result`, take the returned
`remote` and `candidate_terminal_thread_id` and invoke the existing explicit
candidate transfer:

```bash
ryeos --project . remote worker pull-result \
  <remote> <candidate_terminal_thread_id>
```

The model name is an ordinary bounded-turn request choice supported by the
pinned Codex version, not a Core default or provider branch. A different
signed project workflow may select a different worker and task schema.

`remote bundle-install` is remote-to-caller import: the caller fetches an
installed bundle from its named remote into the caller's live node. It does
not publish or activate a candidate on the stronger target. Target candidate
activation stays in the target operator's stopped-node package/image/bundle
cutover.

## Reproducible host preparation

Before creating a real cloud machine, decide all external inputs:

- provider/host and spending limit;
- machine architecture, CPU/GPU/device profile, RAM, disk, and network egress;
- base image and access mechanism;
- operator key custody and target node policy;
- model/artifact URL, hash, license, and redistribution permission when a real
  model workload is in scope;
- evidence retention location and deletion period.

Stop before provisioning if any choice or credential is missing.

There are two distinct daemon entry contracts. Native hosts use the supported
administrator command `ryeos node host setup`, which provisions the Lillux
controller, cgroup delegation, protected binding, and service lifecycle. A
container managed by an external supervisor may instead receive the same typed
protected binding at an absolute read-only path and set
`RYEOS_HOST_RUNTIME_BINDING` to that path. The shared image entrypoint then
executes only `ryeosd host-runtime --binding <path>`: it does not run root-side
init, provision authority inside the container, or fall back to ordinary
startup. The external administrator/environment builder owns initialization of
the exact app-root generation and creation of the binding before container
start.

The protected binding and signed node policy are independent requirements. The
binding supplies real controller authority; the policy must explicitly require
process scopes. Either one without the other leaves exclusive sessions
unavailable. The binding validates the exact app-root device/inode, node
fingerprint, controller account, and inherited descriptor authority. Replacing
the container, app-root filesystem identity, host lifetime, UID mapping, or
binding requires explicit administrator reprovisioning; surviving volume bytes
do not transfer host authority.

## Hosted-runtime ownership

An externally supervised runtime does not move host provisioning into the
RyeOS daemon or into an image entrypoint. Keep the ownership layers exact:

- source-local signed `.ai/config`, `.ai/tools`, and `.ai/graphs` objects own
  development inputs, requested products, qualification policy, and evidence;
- an installed Lillux adapter owns OS, OCI, namespace, mount, account, cgroup,
  and enclosing-lifetime mechanics;
- RyeOS retains only the generic protected binding, semantic capabilities,
  execution journals, and fail-closed admission;
- an image owns immutable packaging only; and
- `tests/e2e` owns qualification fixtures, never deployment authority.

In particular, do not add a security-bearing contract below a top-level
`deploy/` directory, teach `ryeosd` to interpret Docker or provider topology,
or construct protected authority from mutable environment variables. A tiny
image shim may invoke the generic protected entrypoint with an
administrator-prepared descriptor. It cannot discover, create, weaken, or
repair that authority.

Lillux's host adapter must return an opaque enclosing-lifetime witness in
addition to process-scope configuration. That witness distinguishes a daemon
restart within one still-authoritative host lifetime from replacement of the
enclosing container or host. RyeOS may retain and report its digest, but must
not parse provider IDs, cgroup paths, systemd units, namespace layouts, or
container metadata to reconstruct it. Recovery may settle an old execution
only from Lillux's authoritative scope recovery or lifetime-death proof.

Externally supervised single-tenant destinations are a separate execution
lane. They may reuse generic placement-incarnation readiness, refusal, cleanup, and
receipt vocabulary, but provider metadata is not an OCI/kernel witness and the
target node cannot attest to its own future death. A source-side provider
adapter may observe and control preconfigured sites without exposing lifecycle
credentials to workers; that observation remains distinct from Lillux
authority and from the target-signed candidate result. A project may select or
narrow a named lifecycle profile. The protected provider account, service/slot
ceiling, image/configuration, credential handle, and mutation operations belong
to the source-node operator installation, not project content. The
generic source contract is
`config:development/ryeos/externally-fenced-worker-runtime`; its staged runbook
is `knowledge:ryeos/development/externally-fenced-worker-runtime`.

An externally fenced destination that supplies neither delegated process
control nor a usable namespace sandbox cannot run the ordinary hosted Codex
authoring profile. It must refuse before provider contact unless an
independently qualified closed-tool profile proves that shell, file mutation,
browser, plugin, MCP, and other ambient command routes are absent. A feature
flag or self-reported tool list is not that proof. Hard-contained hosts
continue to use the ordinary authoring profile; do not weaken it to accommodate
a more limited destination.

Structural qualification and installed qualification are different products.
Image shape, signed inventory, node identity, and refusal behavior can pass a
structural smoke while installed process containment, writer exclusion,
restart recovery, container replacement, and old-lifetime death proof remain
unqualified. Promotion requires every installed claim from the signed
development workflow; a partial smoke must never enable the hosted profile.

On an already-provisioned stronger host, use an ordinary checkout outside the
target app root and pin the source commit:

```bash
git clone <operator-approved-ryeos-origin> ryeos
cd ryeos
git fetch --tags <operator-approved-ryeos-origin>
git checkout --detach <exact-commit>
test "$(git rev-parse HEAD)" = "<exact-commit>"
test -z "$(git status --porcelain=v1)"
```

Keep caches target-local and explicitly bounded. They are performance state,
not execution identity or transfer authority:

```bash
export CARGO_TARGET_DIR=/var/tmp/ryeos-qualification/cargo-target
export CARGO_HOME=/var/tmp/ryeos-qualification/cargo-home
export GATE_BUILD_JOBS=<bounded-jobs>
export GATE_TEST_THREADS=<bounded-threads>
```

These variables describe the host-controlled build/test layer. A pushed
project does not carry `target/`, and a default RyeOS execution sandbox does
not gain access to `/var/tmp` merely because the variable names it. If a build
is itself run as an admitted persistent-session item, use its signed
worker-environment contract: the preparer emits a generic target-bound
environment contribution, the capsule retains only bounded path-free values,
and placement resolves a `runtime_view_directory` below the daemon-owned
`.ai/cache/ryeos-runtime` view. Content-backed values must explicitly name the
pinned content dependency that grants them. The protocol runtime-environment
allowlist and existing subprocess/isolation compiler remain the enforcement
path. An ambient host variable, absolute authored path, project ignore rule, or
credential home is never cache authority. For other execution shapes, use a
disposable project-local build and make no persistent-cache claim until their
signed runtime contract provides equivalent authority.

Record toolchain versions and every command's exit status in an evidence
directory outside the checkout and both app roots. A typical build and focused
test sequence is:

```bash
rustc --version --verbose
cargo --version --verbose
cargo build --release -p ryeos-cli -p ryeosd
cargo nextest run -p ryeos-state project_sync
cargo nextest run -p ryeos-api --test remote_descriptor_admission_e2e
cargo nextest run -p ryeos-api --test remote_import_e2e
```

Use `./scripts/gate.sh` for the final source gate. Use
`./scripts/gate.sh --refresh-bundles` only when signed/bundle-owned content was
changed. Preserve the exact resulting `ryeos` and `ryeosd` SHA-256 hashes and,
when applicable, the signed bundle artifact checksum. Do not copy a random
binary into an existing target node or reinstall the developer's primary node.

## Disposable node lifecycle

For local two-node qualification, create independent target-local directories:

```text
qualification/
  source-home/       source-node/       source.sock
  target-home/       target-node/       target.sock
  source-project/    target-projects/
  evidence/
```

`source-project`, `target-projects`, and `evidence` must be outside both app
roots and synthetic homes. Start each ordinary node with its own `HOME`, app
root, UDS path, and loopback listener. Use `ryeos node status --json` to record
the actual bind address and stable identity. Never point either lifecycle
command at the primary node's app root.

On a real remote host, install the already-qualified candidate through the
host operator's normal package/image mechanism, initialize exactly one target
app root, and run it under the normal supervisor. Candidate installation and
host restart remain operator actions outside the RyeOS remote API.

## Authentication and site identity

Configure the live target from the source, preferably from a separately
delivered descriptor trust pin:

```bash
ryeos remote configure --descriptor ./stronger.remote.yaml
ryeos remote status stronger
ryeos remote doctor stronger
```

The descriptor itself pins only the node signing key and its fingerprint.
Descriptor import verifies those pins against the live `/public-key` response,
then `remote configure` records the discovered principal, signing key,
fingerprint, canonical site ID, and vault fingerprint as one configured
coordinate. Non-loopback traffic requires HTTPS. Run `remote configure`
immediately before qualification and review any identity change out of band.
The stored endpoint is one canonical credential-free base URL; it contains no
query, fragment, user information, control/whitespace characters, or trailing
slash. Public audience discovery and signed requests refuse redirects, so use
the target's final origin directly.
`remote status`, `remote doctor`, and admission validate that complete tuple
before authenticated contact; a mismatch skips every signed status/project
probe, refuses to release an admission token, and fails the helper.

For an ordinary node-principal workflow, the target operator mints a short
lived one-time admission token with only the required scopes, or authorizes
the initiating node key locally. A full-project remote execution generally needs:

```text
ryeos.execute.service.objects/has
ryeos.execute.service.objects/put
ryeos.execute.service.system/push-head
ryeos.execute.service.objects/get
<the exact caps required by the executed item and its children>
```

Never grant wildcards. If the target must use the current legacy bundle-export
path, authorize it on that bundle-exporting node for exactly:

```text
ryeos.execute.service.bundle/export
ryeos.execute.service.objects/get
```

For configured-operator continuity, stop the target and use the documented
offline semantic conversion. The target grant must bind the configured
operator key to the initiating node's canonical `site_id` and exact workflow
scopes; the initiating node key separately receives only
`ryeos.attest.request.forwarded-operator`. The initiating node then co-signs each
exact operator request. Do not use this mode for `remote execute`, admission
claim, or arbitrary delegated callers.

## Product qualification policy and settlement

Product qualification uses the existing signed policy Config to select its
trusted-bundle verifier, subject declaration, allowed claims, static
`verifier_parameters`, and minimum process-settlement authority. The current
reader accepts only `ryeos.product_qualification_policy.v3` and requires an
explicit `minimum_verifier_process_settlement`; predecessor v1 and v2 policies
or an omitted field are refused. A missing settlement decision never acquires
a compatibility default.

A v3 producer scenario may select an independent remote verifier by signed
Bundle binary ref and guest target triple. An absent selection authorizes no
remote verifier, and the producer executable is never used as a fallback. The
protected node profile must separately bind the exact scenario-source digest
and verifier artifact hash. Source capture alone grants neither hosted contact
nor a consumer-runtime qualification claim; the accepted root must retain the
complete signed closure and join the guest lifecycle and applied runtime to
the direct probe before either claim is published.

`trusted_process_group_absent` permits a trusted checker whose direct process
has been reaped and whose recorded process group is absent. Descendants can
create other sessions, so this is not evidence that every descendant was
contained and settled. `scope_empty` requires the stronger owned-scope
observation. The selected requirement is signed policy and acceptance checks
the retained execution evidence against it; a profile cannot manufacture the
observation by declaring the stronger value.

The development-platform, Cargo-vendor, static-link-input, portable-bundle,
native-bundle, Core-seed, and substrate-receipt policy definitions now use v3
with explicit `trusted_process_group_absent`, matching GNU Python's existing
trusted-checker lane. Their fixed claims are unchanged: platform/vendor probe
behavior, static-link byte checks, and release tree/receipt checks do not claim
adversarial whole-descendant containment. Signed checker implementations remain
trusted to interpret observations; subject-executed probes also rely on the
subject performing those probes faithfully. The stronger Codex qualification
profile continues to require `scope_empty`.

These are source-definition corrections, not installed qualification results.
After integration and publication of the changed signed policies, qualification
evidence must match their new identities; old evidence is not reinterpreted
under v3. Retire incompatible execution history through the explicit reviewed
schema-cut path, then requalify affected retained products and reassess
dependent calibration and publication eligibility through their existing
owners. Unchanged subject bytes do not require reproduction merely because
policy identity changed.
Policy compatibility alone does not establish a live qualified result or
complete the release calibration/publication path.

The source regression
[`bundled_qualification_policies.rs`](../../../../crates/state/ryeos-state/tests/bundled_qualification_policies.rs)
reads the authored Standard and bundle-release Configs through the actual
`ProductQualificationPolicy` reader and exercises missing-settlement and v1
refusal. It checks wire compatibility; signatures, installed admission, process
settlement, and live qualification need their separate existing checks.

### Selected dependencies in a protected qualification launch

`external-content.launch-product-qualification` retains required-nullable
`project_context`: an exact snapshot selects read-only pinned authority;
explicit null selects an already authorized projectless fixed-pin verifier.
The optional `auxiliary_product_selections` array carries ordinary exact Root
product coordinates for the verifier's other signed slots. Omission means an
empty array, with no discovery or implicit selection. Projectless launches
refuse a nonempty array.

The daemon derives the subject from the authenticated outer witness and signed
policy. An auxiliary cannot replace that subject. Selection count/size bounds,
canonical order, duplicate refusal and Root-only restrictions are checked
before dispatch. Normal product composition then requires exact coverage of
the signed slots and authenticates every witness, relationship, current binding
and required qualification. A declaration name alone grants no authority.

Cargo-vendor's auxiliary slot is `verifier-platform`; release qualification
Tools use `python`. Each selector contains `target: {kind: root}` and a
`selection` with `declaration_id`, `witness_hash`, `witness_source` and required
nullable `qualification_hash`. Supply the exact qualified dependency already
selected for that workflow. The subject's inner selector stays unqualified;
the resulting proof is published against its outer qualified relationship.

The pinned finalizer uses the admitted Engine and snapshot binding, resolves
all selections through the same composition owner and snapshot resolver, and
reconstructs verifier identity with that complete set. The sealed request and
retained selection projection bind auxiliary qualifications as well as the
subject. Retained matching also compares the receiving witness source; it
cannot substitute a different transfer acceptance. Current eligibility is
checked again during proof construction and consumption.

### Signed development checker expectations

The development-platform policy requires
`ryeos.development_platform_profile.v2`: exact `bootstrap` provenance plus a
closed `tool_versions` map for Cargo, GCC, collect2, linker and archiver. The
signed profile selects bounded stable Rust/Zig release numbers and the exact
other version outputs; the real subprocess probes compare every selected
expectation. Cargo's release coordinate must agree with Rust. The x86-64 GNU
ELF/loader layout, executable roles, testimony formats and closure checks remain
fixed domain semantics. The Tool's public parameter schema admits the required
profile before execution; the checker enforces its cross-field rules as well.

The static-link policy requires `ryeos.static_link_input_profile.v2`: the GNU
`target` and a nonempty bounded `files` map of exact `bytes` and `sha256` values.
The signed policy owns the member set, including versioned archive names. Paths
stay inside the supported flat `usr/lib/x86_64-linux-gnu/` archive/object
namespace and `usr/share/doc/libc6/copyright`; modes, link refusal and aggregate
bounds remain enforced. See the signed Config definitions under
`bundles/standard/.ai/config/ryeos/environments/qualification/` for full examples.

This migration replaces `development_platform_abi_v1` with
`development_platform_profile_v2`; the fixed
`development_platform_target_x86_64_linux_gnu_v1` claim remains. Static byte
conformance uses `static_link_inputs_profile_x86_64_linux_gnu_v2`. That claim
measures the exact admitted profile's closure; it does not establish linker
usability or independently measure the architecture of archive contents. Old
profile schemas refuse, and the new checkers do not emit the replaced fixed
claims. Development and release consumer relationships require the new claims.
GNU Python's fixed zlib 1.3.2 proposition is unchanged.

Supported version/provenance/member/byte changes preserve checker code and
change policy and admitted-parameter identity. Consumers still require the
exact current policy; the claim name alone cannot admit another profile's
evidence. Publish coherent checker/policy/consumer definitions and requalify
affected products; retained old attestations are not reinterpreted. Synthetic
compiler subprocesses and direct static-tree tests establish interpretation,
not a live qualified toolchain or a successful downstream release build.

Platform, Cargo-vendor and static-link Tools pin their bootstrap interpreter
through signed `external_content`. Their checkers consume the daemon-sealed
realization identity instead of maintaining a second hardcoded interpreter
digest. This relies on admitted execution; a direct host invocation with a
caller-written environment cannot establish a RyeOS qualification.

Rollout requires signing changed definitions, installing the compatible
checker/daemon generation, and requalifying affected retained products.
Profile-only edits after that need signing and requalification, without a Rust
rebuild. Direct checker tests and source admission tests establish their stated
boundaries; the operational owner separately records installation, execution,
qualification and downstream publication/activation.

### Development producer expectations

Cargo-vendor finalization selects its lock intent through the existing
`strict_signed_project_bundle` resolver. The Tool has an empty caller schema;
the resolver supplies `resolved_config` from
`bundles/standard/.ai/config/development/ryeos/cargo-vendor-lock-intent.yaml`
inside the admitted project snapshot. It verifies that Config and the source
Standard manifest against the node-trusted installed publisher. Installed
Standard anchors publisher authority; its installed Config is not a fallback
for project selection. The resolved value enters the sealed execution plan.

The finalizer compares a stable bounded read of project `Cargo.lock` with the
independently signed expected digest, retains those exact bytes in the fixed
vendor output and refuses an existing/racing output. Authoring another lock
intent changes signed data rather than a Python digest constant. The vendor
checker separately compares the retained lock with its admitted project.
Local CLI fixtures cannot authenticate their own supplied Config projection;
Engine admission and later consumer acceptance retain those responsibilities.

Static-link assembly uses the same resolver with
`bundles/standard/.ai/config/development/ryeos/static-link-contract-intent.yaml`.
Its independently signed digest selects the body of the project's existing
static-link input contract. The producer reads that contract once through a
bounded, stable, no-follow read, checks the digest and parses those same bytes.
The signed Tool separately pins its source realization, and the qualification
policy separately selects the expected output members. Updating one of these
authorities does not implicitly update the others. A different input product
still requires a coherent signed Tool pin and matching qualification policy.

## Project and workload qualification

Bind one clean source project to one absolute target-local project identity as
an explicit operator action:

```bash
ryeos --project "$PROJECT" remote bind-project stronger \
  --remote-project /srv/ryeos/projects/qualification \
  --sync-scope full_project
```

Then exercise a generic admitted item. The repository helper refuses a missing,
different, or non-`full_project` binding; it does not silently overwrite
operator configuration. It performs the full-project execution, pull-back,
fail-closed best-effort job correlation, exact expected-file transition checks,
and compact evidence retention.

The helper requires Bash, Python 3, Git, `realpath`, and GNU
coreutils/findutils. Before contacting the target it resolves every executable
used after pull-back to a regular path outside the synchronized project,
starts through absolute privileged-mode `/bin/bash` so inherited `BASH_ENV`
and shell functions cannot execute before preflight, clears loader and language
startup injection variables, closes `PATH`, and runs
isolated Python from the evidence directory. A project-local tool shadow is a
hard refusal, not a fallback. It performs no Git operation after the
target-controlled pull-back. Its preflight Git probes explicitly disable the
repository's `core.fsmonitor`, so a prior target-controlled pull cannot turn a
later retry into source-host execution. Qualification also requires an explicit
`RYEOS_APP_ROOT`: the helper canonicalizes and freezes that initiating node root,
proves its node/operator configuration and signing key remain outside the
synchronized project, and freezes the resolved source daemon URL before the
first RyeOS request. A locally discovered daemon is re-proved through the exact
app-root lifecycle authority before every client request; an explicit URL must
use HTTPS except for loopback. CLI audience discovery and every signed request
refuse redirects, so neither origin nor transport can move after this check.
`source-client-authority.json` retains only SHA-256 identities of the frozen
root and endpoint, never their literal values; the helper also strips remote
config paths from its retained list projection. Other operational and failure
artifacts remain raw and can contain endpoint details or absolute paths, so the
complete evidence directory is confidential. The helper never reproduces
RyeOS's platform-dependent default app-root discovery. The evidence parent must already exist
and be operator-selected outside both the checkout and source app root; the
helper creates only the new final evidence directory after those checks.
Invoke it as:

```bash
RYEOS_APP_ROOT=/absolute/source-node-root \
tests/e2e/configured-remote/qualify.sh \
  --remote stronger \
  --project "$PROJECT" \
  --remote-project /srv/ryeos/projects/qualification \
  --item-ref tool:qualification/run \
  --input /path/to/non-secret-input.json \
  --ref-binding model=worker:models/qualified \
  --expect-file "result.json=<sha256>" \
  --evidence-dir /var/tmp/ryeos-qualification/evidence/run-001
```

The helper assumes fresh configuration, an exact pre-existing binding,
authorization, node lifecycle, bundle
activation, and model realization are already complete. It intentionally does
not provision, install, start, stop, authorize, activate, or tear down nodes.
Its item and binding arguments are generic; provider-, model-, Codex-, and
local-inference-specific policy remains in the admitted project/bundle
contract rather than generic runtime or orchestration code.

The helper proves a functional source-node-principal round trip and the
integrity of its source-local operational transcript. The transcript is not a
target-signed qualification receipt. A stronger release/workload claim must
also retain and verify the workload's target-signed receipt, chain head, or
other exact authority defined by that workload. Do not weaken owner-scoped
chain APIs to manufacture that evidence for ordinary `remote execute`.

For a long-running workload, use the configured-operator push/run policy
documented by the remote command reference, retain the returned chain root,
launch and placement IDs, and inspect only that exact authority. After a
completed turn has been fenced, the session is terminated, and its frozen
candidate closure and base have been validated, return that retained candidate
explicitly for source-side review with:

```bash
ryeos --project "$PROJECT" remote worker pull-result stronger <chain-root>
```

The configured target grant must include the exact
`ryeos.execute.service.worker-executions/candidate-result` and object-read
scopes. The source caller needs
`ryeos.execute.service.remote/pull-worker-result`. A retry addresses the same
source-local job from the owner, route, project, site, and chain tuple. It
cannot select a newer candidate, silently advance either project HEAD, or
dispose of the target candidate. The result returns the complete target-signed
candidate testimony alongside the source-local job ID and apply counts so the
evidence can be retained without reading node databases.

This return is not independent task qualification. A `frozen` owner-retained
candidate may be returned for inspection while its target root remains open for
an owner decision. `publish_ready` additionally proves that the separately
admitted evaluator accepted the candidate. Workflows claiming an accepted or
publishable result must require `publish_ready`; workflows only returning an
exact candidate for review must describe the weaker evidence honestly.

For a model qualification, retain the exact signed worker/model refs,
realization receipt/artifact hash, device profile, deterministic prompt/input,
result, target thread ID, and settled tool/effect evidence. For an offline
replay claim, the target operator must disable acquisition/egress and restart
the disposable target between initial realization and replay; do not replace
that fault boundary with a process-local cache check.

## Evidence and recovery

Retain at least:

- source commit and clean status;
- host/toolchain/device profile and build/test logs;
- `ryeos`/`ryeosd` and bundle artifact hashes;
- source and target public identity/status documents and descriptor hash;
- the exact non-secret grant scope lists (never private keys, tokens, vault
  values, authorization files, app roots, or complete model caches);
- project binding and input hash;
- push, result, and pull snapshot hashes;
- source-side `remote_execute` job and attempt IDs plus terminal inspection;
- exact output file hashes and workload-specific signed receipts.

On success, `remote execute` returns the durable source-local `job_id`; the
helper inspects exactly that coordinate through
`service:sync/jobs/inspect`. Exact inspection, not list projection, owns the
canonical operation (`item_ref`, exact `ref_bindings`, target site, and target
project path) and the complete retained-attempt evidence.

The helper records a bounded `service:sync/jobs/list` view before execution
only so a lost command response can be investigated without guessing or
retrying. After an ambiguous transport failure it takes one bounded after
view, selects only newly visible `remote_execute` job IDs, and inspects each
candidate to compare its exact operation. The compact list deliberately does
not disclose canonical operations and is never an exact authority. Turnover
can make failure correlation incomplete, and multiple exact candidates remain
ambiguous. The helper fails closed in both cases and never reads SQLite.

These services require the local
`ryeos.execute.service.sync/jobs/list` and
`ryeos.execute.service.sync/jobs/inspect` capabilities.

Evidence is created with restrictive permissions, but raw status, doctor,
job, workload result, and error responses may contain confidential model
output, endpoint details, public keys, and absolute paths. Store it only in an
operator-approved location. `evidence.sha256` is an integrity checksum, not an
authentic signature or a substitute for target-signed workload evidence.

Failure handling is phase-specific:

| Failure | Recovery |
|---|---|
| Build/test interruption | keep logs; rerun from the same clean commit with the same bounded target-local cache |
| Remote identity mismatch | stop; verify the expected rotation out of band, then explicitly reconfigure/re-authorize |
| Synchronous push/execute transport failure | retain the before/after source job views and inspect a unique new job when one is visible; the last source phase may be knowable but remote acceptance/completion may remain ambiguous, so do not retry automatically |
| Accepted `remote run` transport failure | use only the caller-retained launch ID and exact returned thread coordinate for status/cancel/recovery; never guess ownership |
| Pull clean-base conflict | preserve both snapshots and evidence; restore or commit the source worktree deliberately, then rerun from a clean base |
| Bundle install/cutover interruption | follow bundle transaction reconciliation and the normal stopped-app-root/supervisor restart or rollback procedure |
| Managed external-content activation interruption | inspect and recover its durable activation sync job; do not reinstall around it |
| Target loss | keep source checkout and retained external evidence; rebuild an ordinary target from the pinned commit/artifacts and reissue exact grants |

## Teardown

Teardown is an operator-owned lifecycle operation after evidence has been
copied and verified:

1. stop only the disposable source/target nodes by their explicit app roots;
2. revoke/remove target grants and one-time tokens through the supported local
   operator procedure;
3. remove the named remote/project binding if it was temporary;
4. delete disposable app roots, project materializations, and caches according
   to the declared retention policy;
5. terminate an external machine only through the chosen provider after cost
   and evidence checks.

Never recursively target a home directory, repository root, primary app root,
or unresolved environment variable.

## Relationship to portable worker placement

Ordinary named-remote build/test qualification and synchronous full-project
`remote execute` remain separate from hosted-worker placement. Cross-site
worker continuation already has its own signed transfer/adoption contracts,
source fencing, successor chain-writer authority, recovery state machines, and
crash matrix. This development helper consumes the same configured-remote and
site identity boundaries but must not duplicate, reinterpret, or bypass those
placement authorities.
