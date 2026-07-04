```yaml
category: ryeos/future
name: as-launched-resolution-digest
title: As-Launched Resolution Digest
entry_type: implementation_guide
version: "0.1.0"
description: Persist launch-time item resolution as a braid event so the explain view can render what a thread actually launched with, not a fresh re-resolve.
tags:
  - resolution
  - explain-view
  - provenance
```

# Future: As-Launched Resolution Digest

## Status

Deferred. The item explain view (`view:ryeos/item/explain`) re-resolves the
extends chain at inspection time and renders a "resolved now — may differ
from launch" caveat, because launch-time `ResolutionOutput` is not
persisted anywhere durable. The caveat is honest; this doc records the
design for when honesty stops being enough.

## The gap

`service:items/effective` answers "what does this ref resolve to NOW":
extends-chain ancestors, trust classes, digests, policy facts. A thread's
braid answers "what happened". Nothing answers "what did this thread's ref
resolve to AT LAUNCH" — if a bundle re-publish, project edit, or install
changed the chain between launch and inspection, the explain view shows
the new world while the thread ran in the old one.

## Design

Persist a slim digest at launch as a normal braid event (the same seam the
follow-lineage facts use — durable, replayable, chain-scoped):

- ancestor refs + content digests (the chain as composed),
- composed `policy_facts`,
- effective trust class.

NOT the full composed value — digests are enough to prove identity and
localize drift; the full value is reconstructable from CAS by digest when
needed.

The explain view then gains a thread-scoped mode: render the launch digest
as truth, with an optional "re-resolve now" comparison that highlights
ancestors whose digests changed.

## Trigger

The first time the "resolved now" caveat actually misleads — an operator
debugging a thread against the current chain when the launched chain
differed. Until drift bites in a real investigation, do not build: the
event adds per-launch braid weight for every thread to serve a rare read.
