# Mission Control — The Agent's Self-Awareness

> _One agent. One identity. Many limbs. One place to see the whole self._

RYE OS is a single agent distributed across execution environments. Your Ed25519 signing key is its identity. Your CAS is its memory. Remotes are its limbs. Every entry point — Amp, Claude, CLI, webhooks, cron, and the web UI itself — are all ways of telling the same agent to do something.

Mission Control is the only surface that shows the whole self at once.

It is not a dashboard for monitoring agents. There is one agent. It is not a chat interface. The human talks to the agent through entry points — Amp, Claude, CLI. It is not a code editor. Directives and tools are authored elsewhere. It is not a CAS object browser. Individual objects are implementation details.

Mission Control is the operating system's desktop. Processes running, filesystems mounted, network connections alive. The system watching itself.

---

## Entry Points Are Peers

Every way of reaching the agent is equivalent:

```
Human → Amp (MCP)       ─┐
Human → Claude (MCP)    ─┤
Human → CLI             ─┤── same agent, same CAS, same remotes
Human → Mission Control ─┤
Webhook / Cron          ─┘
```

When a human fires an execution from Amp, it runs on a remote. When a webhook triggers a graph, it runs on a remote. When you press execute in Mission Control, it runs on a remote. The agent doesn't know the difference. The entry point is metadata on the thread — not a different system.

Mission Control is unique only because it shows everything at once. Every other entry point is focused on a single conversation, a single command, a single event. Mission Control shows the totality — every execution from every entry point, every remote, every project, all at once.

---

## The Self

At the top level, you see your identity.

Your signing key fingerprint. Your user space — the directives, tools, knowledge, and config that travel with you everywhere, across every project and every remote. The kernel beneath that — system space, the immutable standard library, the primitives that everything resolves to.

This is the constant. It doesn't change per-project or per-remote. It's the anchor point — the center of the self that everything else orbits.

---

## Your Reach

Remotes are not servers. They're extensions of you — where your hands currently are.

Each remote shows:

- **Alive or cold.** Is it awake? When did it last respond? Is it mid-execution or idle?
- **What's deployed.** Which projects are pushed to it? What version of your user space does it have?
- **Sync state.** Is it current with your local CAS? How many objects ahead or behind?
- **Trust.** Its signing key. Whether it's pinned. When you first trusted it.
- **Capabilities.** What it's good at — GPU, near SES, near database, general purpose.

Think of it like proprioception — you don't monitor your limbs, you feel them. A remote that's drifted three manifests behind your local state should feel like a numb arm. A remote mid-execution should feel like a hand in motion. A cold remote that hasn't responded in hours should feel like a limb that's fallen asleep.

The reach view isn't a server list. It's a body map.

---

## Your Projects

Each project is a coherent workspace — a `.ai/` directory with its own directives, tools, knowledge, and state graphs.

A project exists in one place but can be deployed to many. The project view shows:

- **Where it lives.** Which remotes have this project pushed to them?
- **Version coherence.** Are all remotes running the same manifest? If remote A has manifest `ab23...` and remote B has `cd45...`, that's visible immediately — a split in the project's presence.
- **What's in it.** The items — directives, tools, knowledge — with their types, signatures, executor chains. Not as a file browser, but as a map of capabilities.
- **What it's done.** Recent executions against this project, across all remotes, from all entry points.

A project is a unit of deployment and execution. Seeing it fragmented across remotes at different versions should jump out at you. Seeing it running the same version everywhere should feel settled.

---

## The Now

What's happening right now, across the whole body.

Every in-flight execution, regardless of which entry point triggered it, regardless of which remote it's running on. This is the heartbeat — the thing you look at to know "what am I doing right now?"

### Thread Trees

An execution from Amp spawned three child threads — one on `default`, two on `gpu`. One is done, two are still running. The parent is waiting. You see this as a tree:

```
Thread (from Amp, running on default)
├── Child A (gpu) — completed, 12s, 3 cache hits
├── Child B (gpu) — executing node 4/7
└── Child C (default) — queued
```

Every thread carries its lineage. Where it came from (entry point), what spawned it (parent thread), where it's running (remote), what it's doing (item type + ID), how long it's been going, how much it's cost.

### Graph Fan-Out

When a state graph dispatches nodes across remotes, you see the topology:

```
fetch_data (data remote) ──→ train_model (gpu remote) ──→ send_report (email remote)
                                                    ↑
                          classify (default) ────────┘
```

Nodes that resolved from cache glow differently from nodes that actually fired. Independent branches running concurrently on different remotes show their parallelism. You can see the shape of the work — where it's dense, where it's sequential, where it's waiting.

### Budget

Cost ticking in real-time. Per-execution, per-thread, per-remote. A parent thread with a $3.00 budget spawning $0.10 children — you see the budget draining and you see each child's share. This isn't an afterthought billing page. It's part of the live state. How much is this costing me _right now_.

---

## What Just Happened

The recent past. Not a log — a delta.

- **Completed executions.** What finished, what it produced, how long it took, whether it hit cache.
- **New CAS objects.** Results, snapshots, knowledge files that were created.
- **Results not yet pulled.** Execution outputs sitting on remotes that haven't synced back to local. The agent produced something out there — you haven't brought it home yet.
- **Space changes.** What changed in a project's space after an execution ran. Before/after. The diff of the self.
- **Failures.** What went wrong, where, why. Not buried in logs — surfaced as first-class events.

The delta view answers: "I looked away for an hour. What happened?"

---

## What's Coming

The anticipation layer.

- **Scheduled executions.** Cron jobs approaching. The daily email digest at 8am. The weekly report generation on Monday.
- **Bound webhooks.** Which webhooks are wired to which remotes. What events they're listening for. The email agent's inbound pipeline — SES → Lambda → webhook → remote.
- **Pending triggers.** Events that have arrived but haven't fired yet. An email sitting in the queue.

This is forward awareness. You can see what the agent is about to do before it does it. You can intercept — cancel a scheduled run, unbind a webhook, hold a trigger.

---

## Trust

Trust is not an admin panel buried in settings. It's a core layer of self-awareness, always visible.

- **Remote key pinning.** Each remote has its own signing key. You pinned it on first contact (TOFU). It's shown alongside the remote — fingerprint, when you pinned it, whether it's changed.
- **TOFU moments.** When you connect to a new remote, or a remote's key changes, that's a decision point. Not a background log entry — a surfaced event that requires acknowledgement.
- **Signature status.** Across all spaces, all remotes — what's signed, what's unsigned, what failed verification. An unsigned item in a production project should be as visible as a warning light.
- **The trust web.** Your key → items you've signed. Remote keys → items they've signed. The chain of trust from any item back to the identity that vouched for it.

The agent's integrity is not assumed. It's verified, and the verification is visible.

---

## Acting

Mission Control is not read-only. It's a control surface.

- **Execute.** Fire any directive, tool, or graph on any remote. Pick the project context, pick the remote, set parameters, go. Same mechanism as every other entry point — the agent doesn't know this came from the UI.
- **Push / Pull / Sync.** Push project or user space to a remote. Pull results back. Sync CAS. Bring a drifted remote back in line.
- **Kill.** Stop a runaway execution. Cancel a thread tree. Abort a graph mid-walk.
- **Trust decisions.** Approve a new remote key. Reject a changed key. Pin or unpin.
- **Manage triggers.** Create, modify, delete scheduled executions. Bind or unbind webhooks. Pause the inbound pipeline.

Every action taken here flows through the same infrastructure as every other entry point. Push from the UI uses the same sync protocol as push from the CLI. Execute from the UI hits the same remote `/execute` endpoint as execute from Amp. The UI is a peer, not a special case.

---

## What It's Not

**Not a chat UI.** The human talks to the agent through entry points — Amp, Claude, CLI. Mission Control shows you the results of those conversations, not the conversations themselves. You see "a thread was spawned from Amp, it executed these items, it produced these results." You don't see the chat transcript.

**Not a code editor.** Directives and tools are authored in your IDE, committed to your project. Mission Control shows you what's deployed, what's signed, what's running — but you don't write code here.

**Not a CAS object browser.** You don't need to see individual content-addressed objects. You need to see state, flow, and changes. The CAS is the substrate — Mission Control shows you the meaningful patterns in it, not the raw objects.

**Not a log viewer.** Logs are accessible when you drill into a specific execution, but the top-level view is state, not logs. "Thread X is running on gpu" not "2026-03-12T14:23:01 INFO executing node 4."

---

## Relationship to Shard Space

[Shard Space](shard-space.md) is a 3D visualization where every visual element is derived from actual RYE data — CAS objects as shards, relationships as vectors, execution as animation.

Mission Control and Shard Space are the same data, different renderers.

Mission Control is the practical 2D control surface — designed for operating the agent day-to-day. Lists, trees, status indicators, action buttons. Information density optimized for decision-making.

Shard Space is the experiential 3D cosmos — designed for understanding the agent's structure and watching it work. Spatial, immersive, beautiful. Information density optimized for insight and intuition.

They share the same data backbone:

- Space snapshots (what's deployed where)
- Execution state (what's running, what completed)
- CAS topology (objects, manifests, relationships)
- Trust state (signatures, key pinning)
- Sync state (object presence across remotes)

Mission Control comes first because it defines what data matters. The act of building it produces the snapshot extraction, the state projections, the event model that Shard Space will later consume.

Shard Space becomes a view mode within Mission Control. The same information, the same agent, the same self — rendered as a cosmos instead of a dashboard. Flip the switch and the remote list becomes orbital paths, the project spaces become planets, the executions become cascading light. The data doesn't change. The perspective does.

---

## The Core Loop

Mission Control serves one loop:

**See what's happening → see what changed → decide if you need to act → act.**

See: the live state, the recent delta, the upcoming triggers.
Decide: is anything drifted, failed, stale, untrusted, expensive?
Act: push, pull, execute, kill, approve, schedule.

Then watch the result flow through the network — same as you would from any other entry point, but with full visibility into what happens next.

The system watching itself. The agent aware of its own body. One identity, many limbs, one view.
