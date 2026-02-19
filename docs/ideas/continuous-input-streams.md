```yaml
id: continuous-input-streams
title: "Continuous Input Streams"
description: Extend the thread continuation mechanism to handle continuous input — browser sessions, live image flow, and high-volume data streams — using domain-specific summarizers and new hook events
category: ideas
tags: [streams, browser, images, data, continuation, summarizers, hooks]
version: "0.1.0"
status: exploratory
```

# Continuous Input Streams

> **Status:** Exploratory — extends existing thread continuation infrastructure, not scheduled for implementation.

## The Idea

The thread continuation mechanism (`coordination.yaml` → `trigger_threshold: 0.9` → `thread_summary` directive → `handoff_thread` → continuation thread) was designed around conversational context, but it's actually a general solution to a broader problem: any situation where you have more continuous signal than fits in a context window and need to maintain coherent state across the boundary.

The structure is always the same regardless of input type — the context window fills to the threshold, `context_window_pressure` fires, the `thread_summary` directive generates a structured summary, `handoff_thread` spawns a continuation with the summary as seed context, and the stream resumes. The mechanism stays identical. The summary schema adapts to the domain.

This document covers three concrete cases: browser automation, live image flow, and large or fluid data streams.

---

## What Already Exists

The foundation for continuous input streams is already built:

**Thread continuation** — `coordination.yaml` defines the handoff pipeline:

```yaml
# rye/rye/.ai/tools/rye/agent/threads/config/coordination.yaml
coordination:
  continuation:
    trigger_threshold: 0.9
    summary_directive: "rye/agent/threads/thread_summary"
    summary_model: "fast"
    resume_ceiling_tokens: 16000
    summary_max_tokens: 4000
```

**The `thread_summary` directive** — generates structured summaries with Completed Work, Pending Work, Key Decisions & Context, and Tool Results sections. Already uses an XML metadata block with model tier, limits, and permissions:

```xml
<directive name="thread_summary" version="1.0.0">
  <metadata>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="8192" max_spend="0.02" />
    <permissions>
      <execute>
        <tool>rye/agent/threads/internal/*</tool>
      </execute>
    </permissions>
  </metadata>
  ...
</directive>
```

**The `handoff_thread` orchestrator method** (`orchestrator.py`) — six-phase flow: generate summary via directive → fill remaining ceiling budget with trailing messages → build resume_messages → spawn continuation thread → link via thread registry.

**Hook conditions** (`hook_conditions.yaml`) — the `context_window_pressure` event already fires when `pressure_ratio >= 0.8`, triggering a `default_compaction` directive. The existing hook events are: `error`, `limit`, `context_window_pressure`, `after_step`, `after_complete`, `directive_return`.

**Lifecycle events** (`events.yaml`) — `thread_started`, `thread_completed`, `thread_suspended`, `thread_cancelled`, plus cognition and tool call events with criticality levels (`critical` vs `droppable`).

The core insight: **none of this is conversation-specific.** The same pipeline can process any continuous input that exceeds a context window, provided:

1. A domain-specific summarizer replaces the generic `thread_summary` directive
2. The summary schema captures the right state for that domain
3. The continuation thread can resume from the summary alone

---

## What This Proposal Adds

Everything below is **new** — proposed extensions to the existing infrastructure:

| Component | Type | Purpose |
| --------- | ---- | ------- |
| `browser_summarizer` | Proposed directive | Browser session summaries at handoff boundaries |
| `image_summarizer` | Proposed directive | Visual state summaries from frame batches |
| `stream_summarizer` | Proposed directive | Data stream processing summaries |
| `stream_source` hook event | Proposed event | New hook condition for stream-type inputs |
| `summary_directive` override | Proposed config | Per-directive summarizer selection in `coordination.yaml` |
| Shared thread memory store | Proposed infrastructure | Indexed summary store for RAG over past sessions |

---

## Extending coordination.yaml

The existing `continuation` config uses a single `summary_directive` for all threads. For domain-specific streams, the directive itself would specify which summarizer to use via an override. The proposed extension:

```yaml
# Proposed addition to coordination.yaml
coordination:
  continuation:
    trigger_threshold: 0.9
    summary_directive: "rye/agent/threads/thread_summary"  # default
    # Per-directive overrides — keyed by directive name
    summary_directive_overrides:
      "browser/automate": "rye/agent/streams/browser_summarizer"
      "vision/monitor": "rye/agent/streams/image_summarizer"
      "data/process": "rye/agent/streams/stream_summarizer"
```

This follows Rye's existing pattern — `summary_limit_overrides` already exists in `coordination.yaml`. The override is resolved by `handoff_thread` before spawning the summary thread, and the orchestrator already loads `summary_directive` from `cont_config`.

---

## Browser Automation

### The Problem

A browser automation session generates a continuous stream of events — page loads, DOM changes, clicks, form interactions, network requests, error states. A complex task might involve hundreds of meaningful events across a long session. You can't hold all of that in a single context window, but you also can't lose the thread of what's happening mid-task.

### How It Works

Events stream into the context window — page state, actions taken, results observed. When `context_window_pressure` fires (existing hook, `pressure_ratio >= 0.8`), the orchestrator's `handoff_thread` runs the `browser_summarizer` directive instead of the generic `thread_summary`. The summarizer captures current browser state completely enough that the continuation thread can act immediately without re-exploring.

The `browser_summarizer` would be a standard Rye directive — same XML metadata structure as `thread_summary`:

```xml
<!-- Proposed: rye/rye/.ai/directives/rye/agent/streams/browser_summarizer.md -->
<directive name="browser_summarizer" version="1.0.0">
  <metadata>
    <description>Summarize a browser automation session for thread handoff.
    Produces state-oriented summaries that let the continuation thread
    act immediately.</description>
    <category>rye/agent/streams</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="8192" max_spend="0.02" />
    <permissions>
      <execute>
        <tool>rye/agent/threads/internal/*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="transcript_content" type="string" required="true">
      The browser session transcript to summarize
    </input>
    <input name="directive_name" type="string" required="true">
      Name of the directive this thread was executing
    </input>
    <input name="max_summary_tokens" type="integer" required="false">
      Target maximum tokens for the summary output (default: 4000)
    </input>
  </inputs>
</directive>
```

The summary schema is state-oriented rather than task-oriented:

```
## Task
Complete checkout for cart containing 3 items on shop.example.com.

## Current State
Checkout page — payment form visible. Cart confirmed correct.
Shipping address populated successfully. Credit card fields visible,
autofill attempted twice.

## What Was Tried
First autofill attempt: all fields populated but expiry rejected.
Second attempt: same result. Error message: "Invalid expiry format."

## Hypothesis for Next Thread
Expiry field expects MM/YY format. Current autofill is passing MM/YYYY.
Reformat before next attempt.

## Pending
Submit payment once expiry resolved. Handle any post-submission redirect.
```

The continuation thread reads this summary as its seed (injected by `handoff_thread` as "Thread Handoff Context"), knows exactly where it is, and acts. No re-exploration, no re-discovery of state already established.

### Future: RAG Over Past Sessions

This proposal does **not** include a shared thread memory store — that's a separate piece of infrastructure that doesn't exist yet. But the summary format is designed with future indexing in mind. If a summary store with embedding search is built later, browser session summaries would be indexed automatically — surfacing past sessions when the current session hits a similar situation (same checkout flow, same error pattern, same login sequence).

---

## Live Image Flow

### The Problem

Live image input — browser screenshots, screen recordings, camera feeds, document scans — is expensive context. A screenshot every few seconds is not something you can continuously inject into a context window. But the agent needs to maintain awareness of what's happening visually across frames.

### How It Works

Images are processed within the context window — described, compared, state tracked. Because images are token-expensive, the `context_window_pressure` threshold will be hit sooner. The `image_summarizer` directive fires at handoff and captures visual state as structured text. The images themselves don't persist into the continuation — only the description does.

The `image_summarizer` follows the same directive pattern:

```xml
<!-- Proposed: rye/rye/.ai/directives/rye/agent/streams/image_summarizer.md -->
<directive name="image_summarizer" version="1.0.0">
  <metadata>
    <description>Summarize visual state from image-heavy sessions.
    Converts frame observations into structured text that persists
    across thread boundaries without carrying the original images.</description>
    <category>rye/agent/streams</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="8192" max_spend="0.02" />
  </metadata>

  <inputs>
    <input name="transcript_content" type="string" required="true">
      The image session transcript to summarize
    </input>
    <input name="directive_name" type="string" required="true">
      Name of the directive this thread was executing
    </input>
    <input name="max_summary_tokens" type="integer" required="false">
      Target maximum tokens for the summary output (default: 4000)
    </input>
  </inputs>
</directive>
```

The summary captures current visual state completely:

```
## Visual State at Boundary
Browser: checkout page, payment form.
Visible elements: card number field (populated), expiry field (validation
error highlighted in red), CVV field (empty), submit button (disabled).
Last frame timestamp: 2026-02-19T11:42:03Z

## Changes Observed This Thread
- Page loaded cleanly, no network errors
- Autofill triggered at T+4s, populated card number and name correctly
- Expiry field rejected input at T+7s, error state appeared
- No other UI changes detected

## Relevant Visual Patterns
Expiry field uses inline validation (error appears on blur, not submit).
Submit button remains disabled until all fields valid.

## Next Thread
Retry expiry with MM/YY format. Watch for submit button enabling.
```

The continuation thread has a complete picture of current visual state without any of the original frames. Frames are discarded after processing — only the structured description persists.

### Tighter Thresholds for Image Tasks

Image token cost means `context_window_pressure` will fire earlier in practice, even at the default `pressure_ratio: 0.8`. A directive processing dense UI screenshots might need a project-level hook override with a lower threshold. This is already possible with the existing hook system — user hooks at layer 1 override builtin hooks at layer 2:

```yaml
# Proposed: project-level hook override in .ai/hooks/
- id: "image_context_pressure"
  event: "context_window_pressure"
  layer: 1
  condition:
    path: "pressure_ratio"
    op: "gte"
    value: 0.6  # tighter for image-heavy sessions
  action:
    primary: "execute"
    item_type: "directive"
    item_id: "rye/agent/streams/image_summarizer"
    params:
      pressure_ratio: "${event.pressure_ratio}"
```

---

## Large and Fluid Data Streams

### The Problem

Some tasks involve data too large to hold in context (a full database export, a live API feed, a document corpus) or that changes continuously (a live market feed, a monitoring stream, a real-time log). The agent needs to reason about this data without being able to hold all of it at once.

### Sequential Processing

For data processed sequentially — working through a log file, analysing a dataset batch by batch — the context window fills with data, hits the threshold, and hands off via the same mechanism. The `stream_summarizer` directive captures processing state and position:

```
## Data Stream
Live API error log for api.example.com — processing from offset 48,203.

## This Chunk (offsets 48,203 – 51,847)
3,644 events processed.
Notable: spike in 503 errors between 10:42–10:51 UTC, 847 errors in 9
minutes vs baseline of ~12/min. Correlated with deploy event at 10:41 UTC.
No recurrence after 10:51.

## Running State
Total errors this session: 12,847. Error rate returning to baseline.
Open hypothesis: deploy at 10:41 introduced a connection pool misconfiguration,
self-resolved after instance recycling.

## Next Chunk
Continue from offset 51,848. Watch for recurrence of 503 spike pattern.
Flag if rate exceeds 50/min again.
```

### Fluid/Live Data

For data that changes continuously rather than being processed sequentially, the summary captures current understanding rather than a processing position:

```
## Current Understanding (as of 11:47 UTC)
BTC/USD trading at 84,203. Volatility elevated, ATR at 2.1x 30-day average.
No clear directional signal.

## What Changed This Chunk
Price moved -2.3% in 18 minutes following macro news release at 11:31.
Volume spiked 4x at release, normalizing now.

## Monitoring State
Watching for support test at 83,400. No action triggered this chunk.

## Next Thread
Continue monitoring. Alert if 83,400 breaks with volume confirmation.
```

The distinction — cursor-based vs stateful — is captured in the summary schema, not in any framework config. The summarizer directive itself decides which format to produce based on the input it receives.

---

## The General Pattern

Across all three cases the same existing infrastructure applies:

```
Input stream (continuous, high-volume, or too large for one context)
        │
        ▼
context_window_pressure fires (hook_conditions.yaml, pressure_ratio >= 0.8)
        │
        ▼
handoff_thread (orchestrator.py) resolves domain-specific summarizer
        │
        ▼
Summarizer directive generates structured summary (domain-specific schema)
        │
        ├── Summary injected as "Thread Handoff Context" user message
        │
        └── Continuation thread spawned with same directive
            └── Summary + trailing messages within resume_ceiling_tokens
                        │
                        ▼
                Stream resumes
```

The mechanism is identical to conversational thread handoff. The summary schema is the only thing that changes per domain. The hook system, event system, orchestrator, and continuation chain are exactly the same infrastructure. No new concepts, no new architecture — just different summarizer directives producing different schemas into the same pipeline.

---

## Summary Schema Design

Getting the summary schema right for a given domain is the main design work. A few principles that apply across all cases:

**State completeness** — the continuation thread should be able to act immediately from the summary alone without re-exploring. If it's not in the summary, the next thread has to rediscover it. The existing `thread_summary` directive already enforces this with its Completed Work / Pending Work structure.

**Action orientation** — the summary should describe not just what happened but what to do next. The "next thread" section is as important as the retrospective sections.

**Appropriate compression** — images and raw events don't need to persist. Structured descriptions of their content do. The summary should be as compact as possible while remaining complete — the existing `summary_max_tokens: 4000` budget enforces this.

**Cursor or state** — sequential streams need a position cursor so the next thread knows where to resume. Stateful streams need a current-understanding snapshot. The schema should make clear which mode applies.

---

## Proposed New Directives

These are **proposed additions** — they do not exist in the codebase today.

| Directive | Proposed Location | Purpose |
| --------- | ----------------- | ------- |
| `browser_summarizer` | `rye/rye/.ai/directives/rye/agent/streams/browser_summarizer.md` | Browser session summaries — state-oriented, captures page state, actions tried, hypotheses |
| `image_summarizer` | `rye/rye/.ai/directives/rye/agent/streams/image_summarizer.md` | Visual state summaries — converts frame observations to structured text |
| `stream_summarizer` | `rye/rye/.ai/directives/rye/agent/streams/stream_summarizer.md` | Data stream summaries — supports both cursor-based (sequential) and stateful (live) modes |

All three follow the same pattern as the existing `thread_summary` directive: XML metadata block, same input interface (`transcript_content`, `directive_name`, `max_summary_tokens`), same integration point (`handoff_thread` calls them via `thread_directive.execute()`). They're thin domain-specific wrappers — they produce summaries in the right schema for their input type, then the existing `handoff_thread` pipeline handles injection into the continuation thread.

---

## Open Design Questions

### Summarizer Selection

How does `handoff_thread` know which summarizer to use? Options:

- **Config-level overrides** (proposed above) — `summary_directive_overrides` in `coordination.yaml` keyed by directive name. Simple but requires explicit registration.
- **Directive-level metadata** — the directive itself declares its preferred summarizer in its XML metadata block. More self-contained but requires a new metadata field.
- **Convention** — if a directive named `foo/bar` exists, check for `foo/bar_summarizer` first. Zero config but implicit.

### Token Budget Tuning

The existing `resume_ceiling_tokens: 16000` and `summary_max_tokens: 4000` work for conversational handoffs. Image-heavy sessions may need different budgets. Should these be overridable per-summarizer, or is the existing config sufficient?

### Shared Thread Memory (Future)

The original vision includes RAG over past session summaries — surfacing relevant past sessions when the current session encounters a similar situation. This requires infrastructure that doesn't exist yet:

- An embedding index for thread summaries
- A retrieval API that queries by current session state
- Integration with the continuation pipeline to inject relevant past context

This is a separate proposal. The summarizer directives are designed to produce indexable output, but the indexing infrastructure itself is out of scope here.

### Hook Event Extensions (Future)

The existing hook events (`error`, `limit`, `context_window_pressure`, `after_step`, `after_complete`, `directive_return`) may need stream-specific additions for fine-grained control — events like `stream_chunk_received` or `stream_source_exhausted`. These would be new entries in `hook_conditions.yaml` and `events.yaml`. Whether they're needed depends on how much control the directive needs over the stream lifecycle vs just relying on `context_window_pressure`.

---

## Relationship to Existing Infrastructure

| Existing Component | What It Does Today | How Streams Extend It |
| ------------------ | ------------------ | --------------------- |
| `coordination.yaml` | Defines `trigger_threshold`, `summary_directive`, `resume_ceiling_tokens` | Add `summary_directive_overrides` for per-directive summarizer selection |
| `thread_summary` directive | Generates structured conversational summaries | Domain-specific summarizers follow the same interface |
| `handoff_thread` orchestrator | Summary → trailing messages → continuation spawn | Resolves summarizer override before calling `thread_directive.execute()` |
| `context_window_pressure` hook | Triggers compaction at `pressure_ratio >= 0.8` | Project-level overrides can set tighter thresholds for expensive input types |
| `events.yaml` lifecycle events | `thread_started`, `thread_completed`, `thread_suspended` | Future: stream-specific events for fine-grained control |
| Three-tier space system | project → user → system resolution | Stream summarizers resolved through the same space hierarchy |
