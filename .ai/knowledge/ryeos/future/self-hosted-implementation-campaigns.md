<!-- ryeos:signed:2026-08-27T04:21:34Z:a7f87f95de7989f8cbdf1beada8de88b0ab36a91b2fe03d5ccf777b8799ae20f:KabWaG5J+x9tS0c2mVMtfXy1OJaBdcE1iKfmjJjoifo6g9zOv32yDf/YWPa2LU4j0aKW3PDabBRuyLLkjAvOBw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
category: ryeos/future
name: self-hosted-implementation-campaigns
title: Self-Hosted Implementation Campaigns
description: Scheduled design for RyeOS-owned workers implementing the next RyeOS generation under a strict host/candidate trust boundary
entry_type: design
version: "0.2.0"
```

# Self-hosted implementation campaigns

## Status

Scheduled next consumer of the landed generic hosted-worker and portable
placement substrate. Release qualification of the current worker execution,
private candidate workspace, recovery, and explicit publication path remains
the immediate gate. This document does not describe an unbounded autonomous
loop or grant deployment authority.

## Purpose

RyeOS should eventually account for the execution that changes RyeOS itself.
An admitted `worker_execution` should be able to own a durable implementation
goal, edit a private RyeOS candidate, run bounded builds and checks, retain its
evidence, survive interruption, and return a frozen candidate for explicit
operator disposition.

This is more than leaving an external coding client running. RyeOS owns:

- the exact worker program and signed workload policy;
- the admitted starting project generation;
- the goal, command, observation, approval, and child-execution braid;
- the private copy-on-write implementation workspace;
- resource, duration, spend, capability, and project bounds;
- restart recovery and portable placement handoff;
- build, test, review, and comparison evidence; and
- candidate validation plus explicit publication or discard.

The first signed workload may be Codex App Server, but this is a generic
worker-execution design. No Codex, provider, or model branch belongs in the
engine.

## Relation to adjacent future work

This document owns implementation-campaign execution. It deliberately does
not absorb two adjacent directions:

- `knowledge:ryeos/future/ryeos-native-development-platform` owns future
  project hosting, check records, review objects, artifacts, releases, and Git
  projection;
- `knowledge:ryeos/future/reflexive-deployment` owns an admitted activation
  set that drains the old epoch, installs a promoted candidate, validates the
  new boot, and records deployment evidence.

The boundary is:

```text
implementation campaign
  -> frozen, checked candidate

development/review platform
  -> accepted change and release candidate

reflexive deployment
  -> explicitly authorized activation
```

An implementation campaign never becomes deployment authority merely because
its checks passed.

## Foundational trust cut

Self-hosting must not become live self-modification. One installed RyeOS
generation remains authority for the complete campaign that proposes its
successor:

```text
installed RyeOS generation N
  `-- worker_execution
        `-- private candidate for RyeOS generation N+1
              |-- source edits
              |-- builds and focused checks
              |-- candidate-node probes in disposable state
              `-- frozen candidate

operator/release authority
  `-- validate and promote N+1

installed RyeOS generation N+1
  `-- may host the next campaign
```

Generation N+1 is data and child execution from the perspective of generation
N. Candidate binaries, schemas, bundles, or tests cannot alter the authority
of the host that evaluates them.

The implementation worker must not receive authority to:

- replace installed RyeOS binaries or registered bundles;
- stop, restart, signal, or reconfigure its hosting daemon;
- mutate the host app root, runtime databases, node policies, grants, trust,
  credentials, or external-content bindings;
- access operator, publisher, node, vault, or deployment signing keys;
- run privileged installation or obtain ambient `sudo` authority;
- publish its own candidate or advance the live project HEAD; or
- interpret a candidate process response as testimony from the host node.

Release signatures are authored only after review by existing operator or
release authority. A disposable candidate-node test may use an ephemeral
test signer, but its identity must be explicit in the evidence and can never
stand in for a production publisher or node signer.

## Initial execution topology

The first useful shape is a direct worker root, not a new campaign engine:

```text
configured owner
  `-- worker_execution root
        |-- one pinned private CoW RyeOS workspace
        |-- one owner-issued implementation goal
        |-- one long-running admitted worker turn
        |-- ordinary edit/shell/build/test operations
        |-- optional lineage-linked RyeOS child executions
        `-- frozen candidate -> validate -> publish | discard
```

The worker uses its ordinary admitted development tools inside the private
workspace. A shell command does not need a new daemon action merely because a
worker issued it. RyeOS-specific graph, receipt, comparison, or local-inference
operations may be exposed later through the smallest existing project-scoped
callback/client authority that proves the workload needs them.

The worker receives neither an operator private key nor an unrestricted daemon
socket. Any project-scoped RyeOS capability binds at least:

```text
chain_root_id
+ placement_thread_id
+ worker_boot_epoch
+ exact project authority
+ signed allowed operation/ref ceiling
```

Calls made through that capability create ordinary child threads and receipts.
They do not create a second implementation journal or a hidden orchestration
plane.

## First overnight acceptance

The first overnight capability is one bounded owner-issued turn. It is not an
automatic prompt loop.

1. Admit an exact RyeOS project HEAD into a private CoW workspace.
2. Start one `worker_execution` and issue one explicit implementation goal
   with mechanical acceptance criteria.
3. Disconnect every initiating client.
4. Let the worker inspect, edit, build, and run focused checks normally.
5. Retain commands, observations, child executions, usage, failures, and
   candidate facts in the root chain.
6. Restart the hosting daemon once and recover the same root, workspace,
   upstream workload thread, and credential generation under a new worker boot
   epoch.
7. Stop honestly on completion, explicit interruption, hard lifetime, budget,
   integrity refusal, or a durably waiting approval.
8. Freeze the resulting candidate without changing the live project HEAD.
9. Let an authorized returning operator inspect evidence and explicitly
   validate and publish or discard the candidate.

If the workload voluntarily completes its turn, RyeOS does not infer that it
should be prompted again. Finishing early is evidence to measure, not authority
for an implicit continuation policy.

## Candidate build and test boundary

Build output is execution state or a declared artifact, not automatically
project content. Generated targets, caches, downloaded dependencies, and test
state remain excluded from the candidate unless an authored contract
explicitly retains them.

Candidate RyeOS binaries may be exercised only as bounded children using:

- an exact candidate binary or bundle closure;
- a disposable app root and runtime directory;
- distinct loopback ports and control sockets;
- no access to host node state, keys, credentials, or policy;
- explicit process-group ownership and cleanup; and
- retained logs, exit status, capsule, and test evidence.

A candidate daemon is not the daemon hosting the campaign. Its identity,
database, trust, and observations belong to the disposable candidate test
boundary.

Focused checks come first. The worker should run the smallest build, unit,
contract, integration, or installed-project probe that proves the current
change. A broad repository test campaign is justified only by the affected
surface or an explicit goal; it is not evidence of diligence by itself.

## Evidence and disposition

The campaign chain should make the implementation legible without treating
terminal scrollback as authority. Retained facts include:

- starting project HEAD and admitted project authority;
- worker-execution exact program, placement thread, capsule, and boot epochs;
- authored goal, bounds, and every owner-issued continuation instruction;
- canonical commands and bounded outputs;
- source changes and candidate snapshots;
- child build, test, graph, inference, and review identities;
- check results, artifacts, costs, and failure classifications;
- daemon/process recovery transitions;
- unresolved approvals or ambiguous possible-contact boundaries; and
- candidate validation, publication, or discard facts.

Successful checks do not publish. Worker completion does not publish. A review
recommendation does not publish. Publication remains an explicit expected-HEAD
compare-and-swap by admitted owner authority.

## Evidence-gated multi-turn campaigns

Only measured failure of the one-turn shape justifies a durable multi-turn
campaign. Examples include a workload that consistently returns with
mechanically unmet acceptance conditions or a real implementation process that
requires independent check/review results before the next edit.

The later controller may be a signed graph or another existing RyeOS execution
shape, but it must remain bounded by authored policy:

- original goal and immutable acceptance criteria;
- maximum turns and continuations;
- wall-time, provider-spend, CPU, memory, and storage ceilings;
- exact allowed project and RyeOS operation surface;
- stop-on-approval and stop-on-ambiguous-side-effect rules;
- one current candidate generation and expected predecessor;
- no automatic publication or installation; and
- a durable reason for every continuation.

A continuation is admitted only from exact retained evidence such as:

- a named focused check failed;
- a review finding remains unresolved;
- a required acceptance condition is mechanically false; or
- the worker explicitly returned a typed incomplete result inside the authored
  continuation policy.

Narrative dissatisfaction, elapsed time, or a generic instruction to “keep
trying” is not sufficient authority. Each issued turn uses an idempotency key
and fences the current chain head, placement, boot epoch, and candidate
generation.

Terminal states include:

- acceptance satisfied;
- worker-declared or mechanically proven blocker;
- approval required;
- budget or deadline exhausted;
- integrity or authority refusal;
- candidate conflict; and
- explicit owner interruption.

The controller records the terminal reason and does not relaunch by inference.

## Review composition

Implementation, checking, and review are separate roles even when the same
workload technology can perform all three.

A later campaign may compose:

```text
implementation worker
  -> candidate
  -> deterministic checks
  -> independent review worker(s)
  -> finding admission
  -> bounded correction turn or frozen result
```

Review workers receive the candidate and declared evidence closure, not the
implementation worker's ambient private state. Review output is a proposal
until deterministic contract checks or admitted owner policy accept it. A
model assertion that code is safe is never release testimony by itself.

Parallel workers must not write one shared candidate tree. Each receives an
isolated generation or read-only review realization; one admitted merge or
conditional publication boundary combines accepted changes.

## Local inference extension

Recorded local inference can expand breadth without changing campaign
authority. It does not wait for sealed qualification. Admitted local workers
may perform:

- codebase indexing and bounded retrieval;
- candidate or test generation;
- static analysis and contract comparison;
- independent review and failure clustering;
- schema-diff or evidence classification; and
- repeated low-cost hypothesis evaluation.

The implementation worker remains responsible for integrating evidence into a
coherent candidate. Local inference uses its own recorded or qualified-sealed
execution contract, effects, model/runtime identity, and accounting. It does
not become correctness authority merely because it is local or reproducible.

The first suitable implementation goal is completion and qualification of the
serious remote tinygrad path itself. During that bootstrap, the hosted coding
workload may still use a frontier subscription while RyeOS owns the candidate,
commands, evidence, recovery, bounds, and publication boundary. After the
operator installs the accepted generation, later campaigns can use the admitted
local model for low-cost breadth. The final offline workload contains neither
the hosted coding session nor remote-provider credentials.

## Federation extension

Federation lets one campaign use specialized nodes while retaining one durable
goal:

```text
implementation placement
  |-- build children on high-core nodes
  |-- local-inference children on GPU nodes
  |-- candidate-daemon probes on clean qualification nodes
  `-- review children on independently admitted placements
```

The implementation session continues to use its stable `chain_root_id`.
Cross-node worker handoff creates a successor `placement_thread_id` under that
chain; it does not create a second campaign identity. Independently running
build, test, inference, and review children retain their own chain identities
and are not silently moved with the implementation worker.

Placement must preserve exact project/candidate authority, admitted program
closure, accounting remainder, credential-subject continuity, and source/
target chain-writer authority. Host-local paths, credentials, process IDs, and
worker boot epochs are not portable identity.

Federation does not authorize a remote candidate node to install itself on the
source or target host. Promotion remains a separate explicit boundary.

## User and field experience

The execution field should show:

- implementation goal and current acceptance state;
- stable chain root, current node, placement thread, and worker boot epoch;
- active turn, last durable progress, and waiting reason;
- project base and candidate generation;
- builds, checks, review findings, and child executions;
- token, cost, CPU, wall-time, and storage usage;
- pending approval or possible-contact ambiguity;
- recovery and handoff transitions; and
- frozen, validated, published, discarded, blocked, or exhausted outcome.

An operator should be able to disconnect, return through the stable chain root,
understand why work continued or stopped, inspect the exact candidate and
evidence, and make the publication decision without consulting a worker's
private conversation sideways.

## Implementation progression

1. Release-qualify one same-node hosted worker with ordinary edit, build, and
   focused test tools in a pinned CoW project.
2. Qualify disconnect, daemon restart, credential fencing, approval recovery,
   frozen-candidate validation, and explicit publish/discard.
3. Prove the same execution lineage can hand off to the explicitly selected
   capable site while preserving candidate and credential authority.
4. Run one bounded overnight implementation turn whose goal is the serious
   recorded tinygrad route and its remote target qualification.
5. Retain child build/check artifacts and prove an independent read-only review
   over the exact frozen candidate.
6. Let the operator validate, publish, install, and qualify that candidate; the
   campaign itself receives none of those authorities.
7. Compose the newly admitted local route for retrieval, testing, review, and
   repeated low-cost hypotheses without changing publication authority.
8. Add the minimum project-scoped RyeOS tool surface only when measured work
   cannot use ordinary hosted-worker tools.
9. Add a bounded multi-turn controller only after one-turn evidence establishes
   the need and supplies mechanical continuation predicates.
10. Integrate accepted candidates with the future RyeOS-native development
    platform while keeping activation under the separately reviewed reflexive-
    deployment contract.

## Acceptance properties

- The installed host remains byte- and authority-stable throughout candidate
  work.
- No worker can read release/operator/node credentials or invoke privileged
  installation.
- Every candidate process uses disposable state and cannot impersonate the
  host daemon.
- Client disconnection and daemon restart do not lose or duplicate the active
  implementation turn.
- Every continuation has a bounded policy and durable evidence predicate.
- A pending approval or ambiguous possible contact stops conservatively.
- Parallel workers never mutate one shared candidate tree.
- Build/test/review output is linked to the exact candidate generation.
- Local inference and remote children retain their own execution identity and
  evidence.
- Completion and successful checks never imply publication.
- Validate/publish uses exact candidate closure, base ancestry, and expected
  live HEAD.
- A failed or exhausted campaign leaves an inspectable candidate or explicit
  discard state rather than an unowned working directory.

## Explicit non-goals

- a live daemon rewriting or replacing itself;
- automatic merge, signing, installation, restart, or activation;
- granting a coding worker operator, node, publisher, vault, or deployment
  keys;
- ambient `sudo`, unrestricted daemon control, or node-policy mutation;
- an unbounded recursive prompting loop;
- treating model review as deterministic correctness proof;
- treating candidate-node output as host-node testimony;
- replacing Git interoperability before RyeOS-native project/change objects
  are proven; or
- collapsing implementation campaigns and reflexive deployment into one
  authority.

## Trigger to implement

This trigger is active. Begin only after the current hosted-worker release
qualification is green. The first accepted campaign remains a bounded
single-turn implementation of the local-inference gap; broaden campaign
control only from retained evidence that one turn is insufficient.
