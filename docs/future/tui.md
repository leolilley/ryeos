```yaml
id: tui
title: "TUI — Terminal Control Surface"
description: The terminal-native control surface for RYE OS. A standalone Rust binary consuming the shared input layer and ryeosd streaming infrastructure. Not a chat interface — a cockpit.
category: future
tags: [tui, terminal, mission-control, streaming, agent, input-layer]
version: "0.1.0"
status: exploratory
```

# TUI — Terminal Control Surface

> **Status:** Exploratory. Depends on the [NL CLI plan](.tmp/nl-cli-plan/) work — specifically the shared input layer, agent kind, and streaming infrastructure.

## The Idea

The TUI is a standalone Rust binary (`rye-tui`) that gives you a persistent, multi-pane terminal interface to the running agent. It consumes the same `rye-input` crate as the CLI, the same `/execute/stream` SSE endpoint on ryeosd, and the same agent routing. The difference is shape: the CLI is single-shot, the TUI is persistent. The CLI checks verbs first, the TUI routes everything as natural language by default.

```
CLI:  rye "explain the deploy graph"  → one response → exit
TUI:  [input pane] explain the deploy graph → [output pane streams response] → still open
```

The TUI is the first surface where Mission Control's vision becomes practical. Not the full self-awareness dashboard from day one — but the foundation: see what's running, talk to the agent, watch tool calls happen, browse traces. The cockpit before the flight deck.

---

## What It Shows

### Input Pane

A text input at the bottom. Everything typed here routes through `rye-input::classify()` with `verb_dispatch=false` — all input goes to the active agent as natural language. Verb dispatch is available via explicit prefix (`/execute`, `/fetch`, `/sign`) for power users, but the default mode is conversational.

The input pane carries context. Unlike the CLI's single-shot invocations, the TUI maintains a session thread. Each prompt continues the thread — the agent sees prior context. This uses the same thread continuation mechanism as directive handoff.

### Output Pane

Streamed response from `/execute/stream`. Tokens render as they arrive. Markdown rendering in-terminal — headers, code blocks, lists. Scrollable history of the session's exchanges.

The output pane is not a log. It's a conversation with the agent, rendered as a document. Previous responses scroll up. The current response streams at the bottom.

### Tool Call Panel

Side panel showing tool invocations as they happen. When the agent calls `rye/file-system/read` or `rye/model/invoke` or any other tool, the call appears here with:

- Tool ID
- Parameters (collapsed by default, expandable)
- Status: pending → executing → completed
- Duration
- Result preview (truncated, expandable)

This is the transparency layer. The CLI hides tool calls behind `--debug`. The TUI shows them by default — you watch the agent work. Collapsible for when you just want the response.

### Thread Panel

Active threads on this node. Not just the TUI's session thread — every thread running on ryeosd:

- Thread ID, source (TUI / CLI / MCP / webhook / cron)
- Status: running, completed, continued, cancelled
- Item being executed (directive, tool, graph)
- Duration, spend
- Parent/child relationships (tree view)

This is the first piece of Mission Control. You're talking to the agent in the input pane, and in the thread panel you can see everything else it's doing — the Amp-triggered directive on the other terminal, the webhook-fired graph, the cron job that just ran.

### Status Bar

Bottom bar showing:

- Active agent (`agent:default` or override)
- Project context (resolved project path)
- Node status (ryeosd connected / disconnected)
- Session thread ID
- Cumulative spend for the session

---

## Agent Routing

The TUI's relationship to agents is the same as the CLI's, with different defaults:

```yaml
# ~/.ai/config/input.yaml
tui:
  agent: agent:default
  verb_dispatch: false    # everything is NL unless explicitly prefixed
```

The TUI adds an **agent picker** — switch between agents without restarting:

```
[Ctrl+A] Agent: default ▼
  ├─ default         (general purpose)
  ├─ code-reviewer   (project-scoped)
  └─ ops/deploy      (ops-scoped)
```

Switching agents changes the permissions ceiling and the default directive. The session thread continues — the agent switch is metadata on the next execution, not a context reset.

Project-scoped agents surface automatically. Open the TUI in a project with `.ai/agents/project-assistant.yaml` and it becomes the default. Same three-tier resolution as everything else.

---

## Streaming

The TUI is a streaming-first surface. Every execution uses `/execute/stream`:

```
TUI input
    │
    ▼
rye-input::classify() → NaturalLanguage { prompt, agent }
    │
    ▼
POST /execute/stream → SSE connection
    │
    ├─ event: token    → render to output pane
    ├─ event: tool_call → show in tool call panel
    ├─ event: tool_result → update tool call status
    ├─ event: artifact → notify, offer to execute
    ├─ event: error    → render error in output pane
    └─ event: done     → finalize, store trace ID
```

The SSE event types are defined in the [streaming doc](.tmp/nl-cli-plan/streaming.md). The TUI renders them differently from the CLI — tokens go to a pane not stdout, tool calls get a dedicated panel, artifacts get interactive prompts — but the underlying protocol is identical.

---

## Session Threads

The CLI is stateless by default — each invocation is a fresh execution. The TUI is stateful by default — each input continues the session thread.

```
Input: "what's in the deploy config?"
→ creates thread T-abc, executes, streams response

Input: "add a staging step"
→ continues thread T-abc, previous context available

Input: "now deploy it"
→ continues thread T-abc, full conversation context
```

This uses thread continuation — the same mechanism directives use for multi-turn execution. The TUI's session thread is a daemon thread like any other. It shows up in the thread panel. It has a CAS trace. It's signed.

New session: `Ctrl+N` creates a fresh thread. The old thread is finalized and its trace stored. Thread history is browsable — you can scroll back through previous sessions.

---

## Relationship to the CLI

Same substrate, different shape:

| Aspect           | CLI                           | TUI                              |
| ---------------- | ----------------------------- | -------------------------------- |
| Input layer      | `rye-input`, `verb_dispatch=true` | `rye-input`, `verb_dispatch=false` |
| Execution        | Single-shot                   | Session (thread continuation)    |
| Streaming        | Tokens to stdout              | Tokens to output pane            |
| Tool calls       | Hidden (--debug)              | Visible (side panel)             |
| Thread visibility| Own execution only            | All threads on node              |
| Agent switching  | `--agent` flag per invocation | Interactive picker               |
| Verb dispatch    | Default on                    | Default off (prefix available)   |

Both consume the same daemon endpoints. Both route through the same agent resolution. Both produce the same CAS traces. The TUI is not a wrapper around the CLI — they're peer consumers of the same infrastructure.

---

## Relationship to Mission Control

[Mission Control](mission-control.md) describes the full self-awareness surface: identity, remotes as a body map, project deployment state, trust web, budget, scheduled triggers, the lot.

The TUI is not Mission Control. It's the subset that fits in a terminal:

| Mission Control Concept | TUI Coverage                                      |
| ----------------------- | ------------------------------------------------- |
| The Now (live threads)  | ✅ Thread panel shows all threads on this node    |
| Budget                  | ✅ Session spend in status bar, per-thread in panel |
| Acting (execute)        | ✅ Input pane routes to agents                     |
| Acting (kill)           | ✅ Kill threads from thread panel                  |
| Your Projects           | Partial — active project in status bar             |
| Your Reach (remotes)    | ❌ Not in v1 — requires remote forwarding          |
| Trust                   | ❌ Not in v1 — signatures visible but not managed  |
| What's Coming (triggers)| ❌ Not in v1 — requires webhook/cron visibility    |
| The Self (identity)     | Minimal — fingerprint in status bar                |

Mission Control's full vision needs a richer surface — likely a web UI or the [Shard Space](shard-space.md) renderer. The TUI covers the operational core: talk to the agent, see what's running, kill what's broken.

As Mission Control matures, the TUI grows panels. Remote status appears when remote forwarding lands. Trust management appears when multi-principal auth is enforced. The TUI doesn't need to be designed for the full vision — it grows into it.

---

## Crate Structure

```
rye-tui/
├── Cargo.toml
└── src/
    ├── main.rs          ← entry point, terminal setup
    ├── app.rs           ← application state, event loop
    ├── input.rs         ← input pane, rye-input integration
    ├── output.rs        ← output pane, markdown rendering
    ├── tools.rs         ← tool call panel
    ├── threads.rs       ← thread panel, daemon polling
    ├── status.rs        ← status bar
    ├── stream.rs        ← SSE consumer, event routing to panes
    └── agent.rs         ← agent picker, config resolution
```

Dependencies:
- `rye-input` — shared input classification
- `ratatui` — terminal UI framework
- `crossterm` — terminal backend
- `tokio` — async runtime
- `reqwest` or `hyper` — SSE streaming client
- `pulldown-cmark` — markdown to terminal rendering

Workspace member alongside `ryeosd`, `ryeos-cli`, `rye-input`, `ryeos-engine`.

---

## What It's Not

**Not a chat interface.** It has conversational continuity through thread continuation, but it's not Slack or Discord. There are no message bubbles, no typing indicators, no emoji reactions. The output pane is a streaming document, not a chat log.

**Not a code editor.** You don't write directives or tools here. You talk to the agent, you watch it work, you see what's running. Authoring happens in your editor.

**Not a replacement for Amp/Claude.** Those entry points give you the full MCP tool surface of your IDE. The TUI gives you a persistent terminal presence for the agent. Different contexts, same agent.

**Not Mission Control.** Mission Control is the full self-awareness surface. The TUI is the terminal-shaped subset — the operational cockpit, not the strategic dashboard.

**Not required.** The CLI works. Amp works. The TUI is for when you want to keep a terminal open with a live view of the agent — a persistent presence rather than single-shot commands.

---

## Dependencies

```
Phase 1: Rust CLI (verb parity)
    └─ Phase 2: Agent Kind + Input Layer (rye-input crate)
         └─ Phase 3: Streaming (/execute/stream SSE)
              └─ Phase 8: TUI
                   └─ MCP Unification (for full thread visibility)
```

The hard dependencies are the input layer (Phase 2) and streaming (Phase 3). Without `rye-input`, there's no shared classification. Without `/execute/stream`, there's nothing to render in real-time.

The thread panel's completeness depends on [MCP unification](daemon-runtime-completion.md) — until all execution paths route through ryeosd, the TUI only sees daemon-initiated threads, not MCP-triggered ones. This is acceptable for v1 but incomplete for Mission Control.

---

## Implementation Path

### v1: Streaming Conversation

Input pane + output pane + status bar. Talk to the agent, see streamed responses, session continuity via thread continuation. Agent picker. This is useful on its own — a persistent terminal presence for the agent.

### v2: Transparency

Tool call panel. Watch the agent work — see every tool invocation, parameters, results. Collapsible. This makes the TUI a debugging surface, not just a conversation surface.

### v3: Awareness

Thread panel. See all threads on the node — from every entry point. Kill threads. See parent/child trees. This is the first real Mission Control piece — awareness of the whole self, not just your conversation.

### v4: Growth

Panels for whatever Mission Control surfaces land: remote status, trust state, budget dashboards, trigger management. The TUI grows as the daemon grows. Each new daemon capability gets a panel.

---

## Prior Art

| System             | Shape                                          | Difference                                                    |
| ------------------ | ---------------------------------------------- | ------------------------------------------------------------- |
| **lazygit**        | TUI over git CLI                              | Same pattern — persistent terminal UI over CLI primitives      |
| **k9s**            | TUI for Kubernetes                            | Closest analogue — operational cockpit for a running system    |
| **Warp**           | AI-enhanced terminal                          | Terminal emulator with AI bolted on. TUI is not a terminal.    |
| **Claude Code TUI**| Conversational coding in terminal             | Chat-shaped. No system awareness, no thread visibility.        |
| **bottom/htop**    | System monitoring TUI                         | Read-only monitoring. TUI is bidirectional — observe and act.  |

The k9s analogy is closest. k9s doesn't replace kubectl — it's a persistent operational surface over the same API. The TUI doesn't replace the CLI — it's a persistent operational surface over the same daemon.

---

## Relationship to Other Documents

| Document                                                    | Relationship                                                          |
| ----------------------------------------------------------- | --------------------------------------------------------------------- |
| [Mission Control](mission-control.md)                       | TUI is the terminal-shaped subset of Mission Control's vision         |
| [Daemon Runtime Completion](daemon-runtime-completion.md)   | Thread visibility depends on MCP unification                          |
| [Natural Language CLI](natural-language-cli.md)              | Conceptual foundation — NL fallthrough, model as tool, signed traces  |
| [Workspaces](workspaces.md)                                 | Workspace-scoped views (Phase 5 of workspaces)                        |
| [Shard Space](shard-space.md)                               | Alternative renderer — same data, 3D cosmos instead of terminal panes |
| [Rust Engine Rewrite](rust-engine-rewrite.md)               | Enables true process supervision visible in thread panel              |
