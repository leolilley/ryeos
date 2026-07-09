<!-- ryeos:signed:2026-07-09T01:23:50Z:f037b01dc4ec84c4c0d25acce18b34a9ccafe6368486c3da6ad33589999d5551:vm3WI5erEWX20gNjsPgV3FAPqzoRPxQfHVr2Vs/2MpH0YuG7cZhJKZ0rvpWqCDIV28Y0EevX1+4KUCiGluMfBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
RyeOS UI is a project cockpit over durable threads.
Threads are the execution objects.
Chat, transcript, artifacts, files, and inspectors are lenses.
Project and node are scopes.
```

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
|   |-- Cockpit
|   |   |-- Left: Current Project Live Threads
|   |   |-- Right: Selected Thread Lens
|   |   |-- Bottom: Routed Input
|   |   `-- Top: Node / Project Status
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

Project scope is the default for cockpit driving. Node scope is the broader
operator view. Node-wide activity is not a separate object called "fleet" or
"activity"; it is the same thread list with a wider scope.

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
source:
  ref: service:ui/ryeos-ui/threads/list
  params:
    project: current
    project_path: ""
```

`view:ryeos/node/threads/history` is node-scoped:

```yaml
source:
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

`view:ryeos/thread/chat` is the planned driving lens. It groups turns, keeps
tool calls collapsed, shows artifact chips, and exposes steer/interrupt/continue
intents. It should feel close to a chat interface while staying explicitly
routed to a thread. Do not surface this ref until its widget/source contract
exists.

`view:ryeos/thread/transcript` is the truth lens. It shows the full tail with
sections, folds, line numbers, cognition in/out, provider stream, tool args,
stdout/stderr, results, artifacts, receipts, cost, and raw event JSON. It is
the right place for forensic detail, diffs, and replay inspection.

Both lenses read the same route coordinates:

```text
input.route.thread
input.route.chain_root
```

The chat lens may summarize events. The transcript lens must preserve the
operator's ability to inspect the full event stream.

## Surface Roles

```text
surface:ryeos/ui/cockpit
`-- project driving surface
    |-- left: view:ryeos/threads/history
    |-- right: selected thread lens, currently view:ryeos/thread/transcript
    |-- bottom: view:ryeos/input
    `-- top: node/project status

surface:ryeos/ui/thread
`-- focused thread surface
    |-- center: view:ryeos/thread/transcript
    `-- bottom: routed input

surface:ryeos/ui/thread-crystal
`-- focused thread surface over the prism backdrop
    |-- center: view:ryeos/thread/transcript
    |-- bottom: routed input
    `-- background: view:ryeos/backdrop/prism

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
`-- read-only/session principal
```

This can later become a client-local source such as
`builtin:ryeos-ui/driving-state`, rendered by a normal `sections` or `table`
view. It should report RyeOS UI decisions, not renderer wrapper nodes.

## References

- `bundles/ryeos-ui/.ai/knowledge/ryeos/ryeos-ui/frame-v1.md`
- `bundles/ryeos-ui/.ai/surfaces/ryeos/ui/cockpit.yaml`
- `bundles/ryeos-ui/.ai/surfaces/ryeos/ui/thread.yaml`
- `bundles/ryeos-ui/.ai/views/ryeos/projects/list.yaml`
- `bundles/ryeos-ui/.ai/views/ryeos/threads/list.yaml`
- `bundles/ryeos-ui/.ai/views/ryeos/threads/detail.yaml`
- `bundles/ryeos-ui/.ai/views/ryeos/chain/timeline.yaml`
- `crates/clients/base/src/ryeos-ui/model.rs`
- `crates/clients/base/src/ryeos-ui/view_model.rs`
- `crates/clients/web/docs/web-parity.md`
