<!-- ryeos:signed:2026-10-06T01:00:58Z:12050c50e57f213e43fd127a7493f05d8c86d0a53b208bc1590f90f882b2c881:hLICCYQ/9UOz3xDUNcR506H7mVUcg/7DCnPyaOaflwNnPdro5oP1XWdAcl9xZkicm9d4sGZ52xLGutRq9tpQAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: coding-worker-interaction-and-capabilities
title: Coding Worker Interaction and Capabilities
description: A consistent coding-worker experience with explicit provider capabilities, understandable outcomes and profile-specific guarantees
entry_type: design
version: "0.1.2"
status: discussion
```

# Coding Worker Interaction and Capabilities

## Direction and timing

Users should be able to send work, answer questions, inspect changes, reconnect,
cancel and understand outcomes without learning each coding agent's protocol.
Product quirks belong at the selected adapter boundary. The common surface must
expose actual supported capabilities and accurately describe what happened.

This is future interaction design for when useful work is driven through RyeOS.
It does not add a new UI delivery milestone to the current modularity and adapter
continuation, or authorize worker execution, a new profile or relaxed admission.
Begin with one usable ordinary worker path and its actual supported controls.
Broader multi-provider presentation follows real consumers.

## Place in the platform direction

The [platform direction and delivery](platform-direction-and-delivery.md) owns
the overall application experience and milestones. This note owns one worker
interaction consumer. A usable existing CLI or client can establish the first
application development path; a complete multi-provider graphical interface
is not its prerequisite. Select these interaction improvements when ordinary
worker use needs them, preserving the active continuation's execution and
qualification scope.

## Existing owners and the remaining gap

The 2026-10-05 documentation inspection found substantial foundations:

| Existing document | Existing responsibility | Relationship to this note |
| --- | --- | --- |
| `bundles/standard/.ai/knowledge/ryeos/core/execution/worker-hosted-execution.md` | Commands, approvals, observations, checkpoints, resume, publication and lifecycle/recovery contracts | Reuse its identities and operations; this note specifies how their meaning should reach users |
| `bundles/standard/.ai/knowledge/ryeos/core/kinds/worker.md` | Authored persistent subprocess, source and environment authority | A coding product remains a selected worker implementation |
| [UI development](../development/ui-development.md) and [UI design system](../development/ui-design-system.md) | Shared UI semantics, renderers, admitted project bindings and existing visual language | Extend those views and binding paths, not a separate coding application shell |
| [Self-hosted implementation campaigns](self-hosted-implementation-campaigns.md) | Bounded work, candidate evaluation, continuation and explicit disposition | Agent completion, evaluation and publication remain distinct |
| [Enduring working environment](enduring-working-environment.md) | Projects and work persist across models, processes and clients | Worker processes remain execution details in a project-centered experience |
| [Hosted-node trust boundaries](hosted-node-trust-boundaries.md) | Trusted application/session operation versus additional containment boundaries | Present the selected guarantees accurately; do not infer them from a control's presence |

These documents describe contracts and design foundations, not current live
acceptance of every Render/Codex or Modal path. The gap is a focused interaction
contract connecting common controls, provider differences and understandable
status. This note creates no replacement execution, approval or state authority.

The language-neutral and native-modularity conversation supplies related design
context. Its current continuation is recorded under
`.tmp/ryeos-modularity-continuation-20261005/`; temporary plans are not durable
runtime contracts. That continuation locates the earlier two future notes under
`.worktrees/preserved-next-changes-20261005/.ai/knowledge/ryeos/future/`:
`native-modularity-and-bundle-composition.md` and
`language-neutral-integration-adapters.md`. This note does not reconstruct or
supersede those preserved documents or their ongoing implementation.

## Interaction principles

1. Make common interaction pleasant and consistent across coding products.
2. Keep provider protocol interpretation and quirks behind their adapter.
3. Describe supported capabilities accurately, including their limitations.

Use structured agent protocols where available. Ordinary terminal sessions
retain their own process and PTY lifecycle; a terminal view does not replace
an admitted worker protocol. Execution remains owned by the selected environment,
while clients present observations and submit authorized actions.

Preserve durable intent, distinct provider completion and operation settlement,
and explicit workspace/conversation restore semantics through existing RyeOS
owners. A consistent interface must retain these distinctions.

## Common interaction, explicit differences

| User intention | Desired common behavior | Difference that must remain visible |
| --- | --- | --- |
| Start work | Select project, worker and applicable execution profile; submit a task | Missing runtime, credentials, capability or admission has an actionable explanation |
| Inspect work | Stream messages, tool activity and changes for the selected execution | Provider reports and platform-observed facts keep their provenance |
| Answer a question | Show required inputs and preserve the answer's request identity | A conversational question is distinct from a request for authority |
| Approve an action | Show the requested operation and scope | Native approval options and IDs retain their actual semantics; no fabricated session-wide grant |
| Change direction | Offer steering, queued follow-up or a later turn as supported | Queuing must not be presented as live steering |
| Reconnect | Recover current authoritative observations and pending requests | Reconnection alone does not restart or resend work |
| Resume | Explain whether this continues a conversation, restarts a process or begins linked work | Model/provider switching is not automatically equivalent continuation |
| Undo or restore | Explain exactly which workspace and conversation state will change | Filesystem restore cannot silently claim conversation rollback or reversal of external effects |
| Cancel | Acknowledge the request and show its actual progress | Requested cancellation, process termination and remote resource settlement are separate facts |
| Finish | Present result, changes and any required evaluation/disposition | Agent completion does not itself establish accepted output or publication |

Capability availability depends on the selected adapter/runtime version, admitted
profile, current authority and session state. Use existing profile/route contracts
where they fit. A future capability projection must be derived from those owners;
it cannot grant authority. The daemon rechecks each action. Unknown or stale
support must refuse visibly rather than fall back to a different provider, host
or weaker profile. Avoid provider-name conditionals scattered through renderers.

Do not flatten every product to the least capable interface. Present common
actions consistently and expose useful provider-specific features explicitly
when admitted. Normalization must preserve native request IDs, permission scope,
ordering and facts necessary to explain the operation.

## Status that users can act on

Present a concise primary status with additional facts where needed: working,
waiting for input, waiting for approval, result available, evaluating, cancellation
requested, stopped, or recovery required. These are presentation concepts, not
a new backend state machine. Derive them from the existing lifecycle, approval,
workspace-disposition and operation records; simultaneous conditions may matter.

For example: “Changes are available; evaluation has not run” or “The worker
stopped; remote cleanup is still unresolved.” A success badge must name what
succeeded. Unavailable actions should explain the missing requirement and a
supported next action without displaying raw receipts as the default workflow.
Keep exact identities and diagnostic evidence accessible when inspecting a
refusal, unresolved resource, conflict or evaluation outcome.

Pending requests survive disconnection according to their actual retained
contract. Submission and response use the existing deduplication and identity
fences; reconnect never synthesizes approval or repeats an uncertain mutation.
Late events update their original admitted project/view attachment, not whichever
project is currently focused. Preserve the existing UI's binding and permission
checks across browser and terminal renderers. Do not leak credentials or private
provider diagnostics into the common transcript.

## Useful delivery with honest guarantees

The Codex discussion exposed a sequencing concern: a first useful worker path
can become coupled to runtime production, remote provisioning, qualification,
candidate fencing and resource settlement all at once. Each requirement needs
an owner and a reason tied to the selected workflow's claims.

Before selecting an interaction slice, identify its useful operation, execution
profile, required guarantees and evidence. Reuse an existing trusted path only
if it actually satisfies that operation's contract. Trusted operation still
requires its applicable admission, identity, lifecycle and recovery rules.
No UI label, missing verifier or inconvenient gate permits downgrading an
already selected profile. A different profile requires explicit supported
selection and truthful limitations; there is no automatic fallback.

Require additional containment, independent evaluation, writer exclusion or
remote settlement when the selected claim depends on them. Keep these facts
separate so unrelated future guarantees do not accidentally become prerequisites
for every interaction feature. This is a scoping discipline, not permission to
waive requirements of current unfinished Render/Modal operations.

## First future slice and acceptance

When ordinary worker use becomes the selected implementation focus:

1. Inventory the existing command/approval/session routes and UI affordances for
   one actual worker and profile; record what is implemented versus qualified.
2. Define a short user journey: start a task, observe work, answer a request,
   inspect changes and understand the terminal outcome.
3. Add supported cancellation and reconnect behavior through existing owners.
   Explain unavailable steering/resume/restore features without emulating them.
4. Use a second differing provider when ready to check that common presentation
   preserves capability differences; the first useful slice need not wait for it.

Acceptance should cover the real path, pending input/approval across reconnect,
duplicate responses, stale project/session bindings, unavailable capabilities,
provider failure and unresolved cleanup. Verify that no action is resent solely
because a client reconnects and no unsupported rollback mutates the workspace.
Record the actual operator steps and whether users can distinguish task output,
evaluation and settlement. Source controls alone do not qualify execution.

This note runs no checks and selects no implementation schedule. Open decisions
are the first worker/profile, existing capability projection to reuse, minimum
useful controls, and how to expose advanced product features within RyeOS's
current UI design. No new scheduler, provider registry or frontend state
authority is proposed.
