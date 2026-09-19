<!-- ryeos:signed:2026-09-19T03:32:19Z:d48c491a5c6fb8f1f22756cff47ae9411cae56e6922aa5d989b101b09952078f:48exl7T3M18Hg7qtI1WWhgIAZfwEMQnvokOKcUyHPFV3a9NSaKHy5zCoxqy5+FpCTZdvlsQasmyjBgfw7b87AQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/ryeos-ui"
name: "navigation-tree-v1"
title: "RyeOS UI Project/Thread/View Navigation Tree v1"
description: "The semantic navigation tree for driving RyeOS UI across projects, threads, thread lenses, project lenses, and node-wide operations"
entry_type: reference
version: "1.0.0"
```

# RyeOS UI Navigation Tree v1

This is the RyeOS UI product tree. It names the objects an operator drives, the
lenses over those objects, and the scopes those lenses can take. Renderer
details stay in client code; authored `view:` and `surface:` items compose
against this tree.

The core rule is:

```text
RyeOS UI is a surface of independently composable view sets over durable logical work.
A chain root is the stable work address; placement threads are execution bodies.
Conversation, activity, evidence, files, and inspectors are lenses.
Project and node are scopes.
```

A worker process, provider thread, placement thread, or worker boot epoch is
not an agent identity. RyeOS agent identity remains the signing principal.
Continuation and handoff therefore update the current placement of one work
chain rather than creating a new assistant in the product model.

## Assistant Work Contract

`surface:ryeos/ui/assistant` composes these signed top-level lenses:

- Development: a nested three-region view set with durable work navigation,
  one explicitly routed conversation, exact retained candidate changes, and a
  bounded live execution field;
- Home: exact operator attention, active executions, recent movement, and
  projects;
- Work: principal-scoped chains grouped by durable chain root;
- Review: pending approvals and redacted decision history;
- Sites: the current node and configured remotes, without inferred readiness;
- Programs: an explicit unavailable state until project-owned programme and
  readiness discovery exists.

Inspecting work retains its stable chain and current placement under
`selection.work`; it does not change the composer. The explicit “Talk to work”
affordance copies those coordinates into `input.route`, where drafts key on the
stable logical-work address. A restart, new worker epoch, or cross-site
continuation must therefore read as movement within the same work, not a new
agent or unrelated conversation.

The Development set uses that one scoped route for its conversation, change
inventory, and execution field. Its left navigator does not manufacture a
project or thread from browser state. Until work is explicitly opened, those
views remain honestly unselected. Candidate changes come from the retained
candidate owner and the execution diagram comes from
`service:ui/ryeos-ui/field/execution`; neither is synthetic renderer content.
The explorer, conversation, changes and execution regions are ordinary view
groups in the signed nested layout tree. They are not fixed Svelte product
panes, and the four supporting slots remain free for independently authored
persistent views.

All sources and affordances come from the daemon-compiled effective signed
surface/view closure. Browser and terminal clients send only a binding digest,
an exact compiled coordinate, and its typed producer payload. Presentation
eligibility (`visible_when`, `enabled_when`, and `disabled_reason`) can make an
affordance understandable, but it never grants authority; the invoked domain
service revalidates ownership, frontier, epoch, and operation fences.

Approval listing and candidate inspection are read-only projections. Approval
resolution uses the existing exact hosted-worker resolver. Candidate evidence
shows retained hashes, completion coordinates, evaluator identity and outcome
without treating model completion as task success. Candidate return,
publication, and discard must remain absent until an existing signed operation
is addressable from the exact source-work coordinate.

## Tree

```text
RyeOS UI
|-- Scope
|   |-- Projects
|   |   |-- List
|   |   |-- Current Project
|   |   |-- Recent Projects
|   |   `-- Project Switch / Open / Add / Forget
|   |-- Current Project
|   |   |-- Overview
|   |   |-- Files
|   |   |-- .ai Space
|   |   |-- Items
|   |   |-- Schedules
|   |   `-- Live Threads
|   `-- Node
|       |-- All Live Threads
|       |-- Thread History
|       |-- Remotes
|       |-- Bundles
|       |-- Maintenance / GC
|       `-- Node Status
|-- Threads
|   |-- Live
|   |   |-- Current Project Live
|   |   |-- Node-Wide Live
|   |   |-- Running
|   |   |-- Queued / Accepted
|   |   |-- Suspended Follow Parents
|   |   `-- Resume Queued
|   |-- History
|   |   |-- Completed
|   |   |-- Failed
|   |   |-- Cancelled / Killed
|   |   |-- Timed Out
|   |   `-- Continued Chains
|   |-- Detail
|   |   |-- Summary
|   |   |-- Status
|   |   |-- Execution Metadata
|   |   |-- Cost / Usage
|   |   |-- Limits
|   |   |-- Facets
|   |   `-- Receipts
|   `-- Lineage
|       |-- Chain
|       |-- Children
|       |-- Follow Child
|       |-- Resume Successor
|       `-- Parent Thread
|-- Thread Lenses
|   |-- Chat
|   |   |-- Operator Turns
|   |   |-- Assistant Turns
|   |   |-- Collapsed Tool Calls
|   |   |-- Artifact Chips
|   |   |-- Error Summaries
|   |   `-- Continue / Steer / Interrupt
|   |-- Transcript
|   |   |-- Full Tail
|   |   |-- Line Numbers
|   |   |-- Turn Sections
|   |   |-- Cognition In
|   |   |-- Cognition Out
|   |   |-- Provider Stream
|   |   |-- Tool Calls
|   |   |   |-- Args
|   |   |   |-- Stdout / Stderr
|   |   |   |-- Result
|   |   |   `-- Failure Detail
|   |   |-- Artifacts
|   |   |   |-- Files
|   |   |   |-- Patches / Diffs
|   |   |   |-- Reports
|   |   |   `-- Links
|   |   |-- Receipts
|   |   |-- Cost
|   |   `-- Raw Event JSON
|   |-- Artifacts
|   |   |-- Published Files
|   |   |-- Diffs
|   |   |-- Generated Outputs
|   |   `-- Open / Copy / Inspect
|   |-- Tools
|   |   |-- Calls
|   |   |-- Results
|   |   |-- Failures
|   |   `-- Retries
|   `-- Graph
|       |-- Current Node
|       |-- Frontier
|       |-- Follow Edges
|       |-- Child Threads
|       `-- Cache / Retry State
|-- Input
|   |-- Route
|   |   |-- New Thread
|   |   |-- Current Thread
|   |   |-- Current Chain
|   |   |-- Current Project
|   |   `-- Selected Object
|   |-- Modes
|   |   |-- Submit
|   |   |-- Steer
|   |   |-- Interrupt
|   |   |-- Continue
|   |   `-- Retry Turn
|   |-- Context
|   |   |-- Project Path
|   |   |-- Selected File
|   |   |-- Selected Item
|   |   |-- Selected Thread
|   |   `-- Mentions
|   `-- Completion
|       |-- Slash Commands
|       |-- Item Refs
|       |-- Thread Refs
|       `-- File Refs
|-- Project Lenses
|   |-- Files
|   |   |-- Tree
|   |   |-- Read File
|   |   |-- Recent Files
|   |   `-- Diffs
|   |-- Items
|   |   |-- Tools
|   |   |-- Directives
|   |   |-- Graphs
|   |   |-- Knowledge
|   |   |-- Services
|   |   `-- Config
|   |-- Context
|   |   |-- Project Snapshot
|   |   |-- .ai Directory
|   |   |-- Knowledge Context
|   |   `-- Ignore Rules
|   `-- Schedules
|       |-- Registered
|       |-- Next Fires
|       |-- Pause / Resume
|       `-- Fire History
|-- Layouts / Surfaces
|   |-- Assistant
|   |   |-- Signed navigation: Home / Work / Review / Sites / Programs
|   |   |-- Center: One selected lens
|   |   |-- Bottom: Routed Input
|   |   `-- Optional context through signed views
|   |-- Thread Chat
|   |   |-- Center: Chat Lens
|   |   |-- Bottom: Routed Input
|   |   `-- Background: Crystal Underlay
|   |-- Transcript
|   |   |-- Center: Full Transcript
|   |   |-- Side: Outline / Artifacts
|   |   `-- Bottom: Filter / Command Input
|   |-- Project Explorer
|   |   |-- Left: Files
|   |   |-- Center: File / Diff / Item
|   |   `-- Right: Related Threads
|   `-- Node Overview
|       |-- All Live Threads
|       |-- Projects
|       |-- Remotes
|       `-- Maintenance
`-- Presentation
    |-- Backdrop
    |   |-- Crystal Scene
    |   |-- Animated Empty Center
    |   `-- Underlay Behind Tiles
    |-- Tile Style
    |   |-- Opaque
    |   |-- Translucent
    |   |-- Minimal Chrome
    |   `-- Focused Border
    |-- Folding
    |   |-- Collapse Turns
    |   |-- Collapse Tool Calls
    |   |-- Collapse Artifacts
    |   `-- Expand Raw Detail
    `-- Navigation
        |-- Focus Tile
        |-- Move Row Cursor
        |-- Activate Row
        |-- Drill Into Thread
        |-- Pop Lens
        `-- Switch Scope
```

## Scope Rules

RyeOS UI has two primary execution scopes:

```text
Current Project
`-- threads whose launch metadata resolves to the current project root

Node
`-- threads across project contexts on this node
```

Project scope is the default for project-bound work. Node scope is the broader
operator view. Node-wide activity is not a separate object called "fleet" or
"activity"; it is the same thread list with a wider scope.

Selecting another project is an authority transition, not renderer-local
navigation. The node pins the selected project, recompiles the same signed
surface under that authority, and mints a successor UI session. Web clients
redeem the successor in the same tab; native clients replace their binding,
source subscriptions, thread tail, and seat generation together. No client
rewrites a project path into requests, and the predecessor binding is never
mutated in place.

The UI should make scope visible whenever a thread list can include more than
one project. Node-wide thread rows must carry a project column. Current-project
thread rows may still carry project metadata for drill-down and diagnostics,
but the project column can be visually secondary.

## View Ref Shape

Refs should name the durable RyeOS object first, then the lens. The concrete
refs in this slice are:

```text
view:ryeos/projects/list

view:ryeos/threads/history
view:ryeos/threads/detail

view:ryeos/thread/transcript

view:ryeos/project/files
view:ryeos/project/items
view:ryeos/project/schedules

view:ryeos/node/status
view:ryeos/node/threads/history
view:ryeos/node/events
view:ryeos/node/remotes
view:ryeos/node/bundles
view:ryeos/node/gc
```

Planned refs keep the same shape, but should not be listed in surfaces until
they have real widgets and backing services:

```text
view:ryeos/thread/chat
view:ryeos/thread/artifacts
view:ryeos/thread/tools
view:ryeos/thread/graph
view:ryeos/project/context
```

Labels may be friendlier than refs. For example,
`view:ryeos/threads/history` can render as "Project / Threads" in the
launcher, because the view itself owns the project scope and the input owns the
active/status/kind/source filters.

## Current Ref Migration

The current tree has been cut to the scoped thread tables. Active-only activity
views are not separate launcher entries; they are filters on the thread table.

```text
Current ref                         Target ref
----------------------------------  ----------------------------------
view:ryeos/chain/timeline           view:ryeos/thread/transcript
view:ryeos/threads/list             view:ryeos/threads/history
view:ryeos/threads/history          view:ryeos/node/threads/history
view:ryeos/files/list               view:ryeos/project/files
view:ryeos/items/space              view:ryeos/project/items
view:ryeos/schedules/list           view:ryeos/project/schedules
view:ryeos/remotes/list             view:ryeos/node/remotes
view:ryeos/bundles/list             view:ryeos/node/bundles
view:ryeos/gc/status                view:ryeos/node/gc
```

`view:ryeos/threads/history` is project-scoped by default:

```yaml
sources:
  default:
    ref: service:ui/ryeos-ui/threads/list
    params:
      project: current
      project_path: "@session:project_root"
```

`view:ryeos/node/threads/history` is node-scoped:

```yaml
sources:
  default:
    ref: service:ui/ryeos-ui/threads/list
    params:
      sort: watch
```

Both thread tables expose the same filter input:

```yaml
input:
  id: filter
  feeds:
    fields:
      - { param: active, label: active }
      - { param: status, label: status }
      - { param: kind, label: kind }
      - { param: requested_by, label: source }
```

## Thread Lenses

A thread is the object. Chat, transcript, artifacts, tool calls, graph state,
and receipts are lenses over the same durable event braid.

`view:ryeos/thread/transcript` is the current conversation/activity lens. Its
signed projection maps cognition, paired tool operations, worker turns,
recovery, approval delivery, candidate milestones, and terminal outcome while
retaining expandable raw event fields. Shared client semantics provide bounded
render windows, turn folding, stable tail behaviour, and target-scoped drafts;
web and terminal render those same semantics.

The lens reads both route coordinates:

```text
input.route.thread
input.route.chain_root
```

Any later compact conversation lens may summarize events, but it must read the
same work route and preserve a path to exact activity/evidence.

## Surface Roles

```text
surface:ryeos/ui/assistant
`-- operator surface
    |-- view-set strip: independently retained compositions
    |-- active view set: one authored layout of mounted views
    |-- optional top/left/right/bottom slots: ordinary supporting views
    `-- navigation: signed destination-to-view bindings

surface:ryeos/ui/thread
`-- focused thread surface
    `-- initial view set
        |-- center: view:ryeos/thread/transcript
        `-- optional conversation input view

surface:ryeos/ui/thread-crystal
`-- focused thread surface over the prism backdrop
    |-- initial view set: view:ryeos/thread/transcript
    `-- backdrop: view:ryeos/backdrop/prism

surface:ryeos/ui/workbench
`-- project work surface
    |-- files
    |-- items
    |-- related threads
    `-- inspector

surface:ryeos/ui/project
`-- active project explorer
    |-- left: view:ryeos/project/files
    |-- center: view:ryeos/project/items
    |-- related: view:ryeos/threads/history
    `-- right: inspector

surface:ryeos/ui/node
`-- node-wide overview
    |-- threads: view:ryeos/node/threads/history
    |-- projects: view:ryeos/projects/list
    |-- events: view:ryeos/node/events
    `-- maintenance lenses

surface:ryeos/ui/atlas
`-- namespace and item topology

surface:ryeos/ui/base
`-- general home/library surface
```

## Ambient Chat

The empty center already draws the backdrop as content. A chat-over-crystal
surface extends that idea: the backdrop remains visible as an underlay while
the chat lens and input sit above it.

The current concrete knob is a normal backdrop plus translucent ambient opacity:

```yaml
backdrop: view:ryeos/backdrop/prism

ambient:
  show_background: true
  opacity: 0.35

style:
  border: hidden
```

When a populated center has a declared backdrop and `ambient.opacity` is between
0 and 1, clients render the backdrop first and then draw the tile tree over it.
Web can express this with real layering and translucent tile panels. Terminal
preserves the already-drawn backdrop cells where tile rendering does not need to
clear them. The contract is "background scene remains alive under the lens", not
pixel-identical alpha blending across renderers.

## Driving Inspector

Amp-style focus trees show framework focus. RyeOS should expose a more useful
RyeOS UI driving tree: what the next input or activation will do.

The inspector should include:

```text
Input Owner
|-- view instance
|-- input id
|-- submit mode
|-- Enter behavior
`-- Alt+Enter behavior

Route
|-- project_path
|-- directive or command target
|-- thread
`-- chain_root

Selection
|-- focused tile
|-- focused view
|-- selected row
|-- activation affordance
`-- open/drill target

Scope
|-- current project
|-- node-wide or project-scoped list
`-- compiled signed surface/view binding and session principal
```

This can later become a client-local source such as
`builtin:ryeos-ui/driving-state`, rendered by a normal `sections` or `table`
view. It should report RyeOS UI decisions, not renderer wrapper nodes.

## References

- `bundles/ryeos-ui/.ai/knowledge/ryeos/ui/frame-v1.md`
- `bundles/ryeos-ui/.ai/surfaces/ryeos/ui/assistant.yaml`
- `bundles/ryeos-ui/.ai/surfaces/ryeos/ui/thread.yaml`
- `bundles/ryeos-ui/.ai/views/ryeos/projects/list.yaml`
- `bundles/ryeos-ui/.ai/views/ryeos/threads/list.yaml`
- `bundles/ryeos-ui/.ai/views/ryeos/threads/detail.yaml`
- `bundles/ryeos-ui/.ai/views/ryeos/chain/timeline.yaml`
- `crates/clients/base/src/ui/model.rs`
- `crates/clients/base/src/ui/view_model.rs`
- `crates/clients/web/docs/web-parity.md`
