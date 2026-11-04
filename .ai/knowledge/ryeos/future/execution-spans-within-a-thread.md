---
tags: [future, lineage, attribution, threads, spans, audit]
version: "0.1.0"
status: design-on-file
description: >
  Spans as the attribution record for work that runs inside a thread without
  custody of its own; why inline in-process children are threads instead, and
  the event-sourced span design kept for the case that needs it.
---

# Execution spans within a thread

## Where this came from

In October 2026 the foundation run found that one launch shape, the inline
in-process service child of a callback action, was recorded as a fresh-root
thread with no edge back to its parent. Follow children, detached children and
inline tool leaves all record their lineage; this one did not. The first
framing was "an inline child has no row of its own, so give it a span". Reading
the code corrected that: the child does have a thread row, a daemon-owned task,
a terminal guard, and a replay and recovery contract. It has custody of its own.
The defect was only the missing edge.

The decision was to record the edge and keep the row: an inline in-process
child is a thread in its parent's chain, with the parent as upstream and a
`dispatch` child link, exactly as inline tool leaves already are. One rule for
every inline child, no second lineage mechanism, no schema movement, and the
stop cascade, replay and ownerless-handler recovery keep walking thread links.

This document keeps the span design for the case it is actually for.

## The rule that decides thread versus span

A **thread** is the unit of custody and scheduling. Anything that has a
lifecycle the node must own, settle or recover (a process, a daemon-owned task
with a terminal guard, a native debit, a resumable continuation) is a thread,
and lineage between threads is an edge in the thread graph.

A **span** is the unit of attribution inside a thread. It records that a piece
of work with its own item ref, timing, cost and outcome ran within a thread's
custody, without being something the node could stop, replay or recover on its
own. A span never carries custody, never debits native capacity, and never pins
recovery.

Folding a thing with custody into a span hides a custody unit inside another
record; that is the silent downgrade the honest-record rule forbids. Giving a
thing without custody a thread row claims custody the node does not hold.
Today nothing in the executor runs inside a thread without custody of its own,
so no span exists yet. Candidates, when they appear: a handler that composes
several in-process calls as one step and wants per-call attribution; a runtime
that runs sub-steps of a node in the same process; retrieval or rendering phases
inside a directive turn whose cost should be visible separately.

## The design kept on file

Event-sourced, so the projection stays rebuildable from the chain.

**Events** on the parent thread's braid, `storage_class: indexed`:
`inline_span_opened` `{schema, span_id, relation, operation_id, item_ref, kind,
requested_by, item_content_hash, trust_class, opened_at_ms}` and
`inline_span_closed` `{schema, span_id, status, outcome_code, error,
result_digest, cost, closed_at_ms}`. Span ids carry their own prefix so they can
never be mistaken for thread ids.

**Projection table** `thread_spans` keyed by span id, with the parent thread
id, chain root id, relation, item ref, kind, status, outcome, error, result
digest, cost fields, open and close times, and the two event hashes as unique
columns so replay is idempotent. The span row is the lineage edge; it never
joins `thread_edges`, which stay thread-to-thread. Cursor reset deletes spans
with the chain. The projection derivation version moves when this lands.

**Write path.** Open under one store lock, refused unless the parent is
running, has no stop intent and the shutdown gate is open; replay returns the
stored response and a pending-unknown opening is a refusal, never a re-run.
Close appends the close fact without the running check, so a parent that failed
mid-span still gets a closed span. Failure names the boundary that refused.
A boot sweep closes spans left open by a crash as failed with a restart reason.

**Read path.** Thread get, chain, execution tree, the UI inspect view and the
client DTOs list spans marked by their relation; chain usage rollups do not
count span cost a second time, the span is attribution only.

**Cost.** The span carries the same cost figure the parent accounts, so cost
flows into the parent through the existing runtime paths (node receipts, the
directive budget accumulator), and the span shows where it came from.

## What this is not

It is not a tier and needs no policy data. It is not a replacement for thread
lineage. It is not a place to put anything the node must be able to stop or
recover; that is a thread.
