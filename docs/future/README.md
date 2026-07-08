# Future RyeOS design notes

This directory holds explicitly deferred design work. It should not contain
completed implementation plans or stale branch notes.

Completed work from the June 2026 planning scratch files includes:

- `node/commands` replacing `node/verbs` as the CLI command surface;
- data-driven command registration policy;
- thin accepted/background `ryeos execute --async` launch;
- project `.ai` deployable surface sync and project schedule reconciliation;
- RyeOS UI Dimension v0 and RyeOS UI remotes services;
- bundle event chains, bundle projection helpers, and bundle outbox helpers;
- local direct install layout updates.

The remaining documents here are follow-up directions that were not part of
those completed landing units.

- [`bundle-events-public-operation.md`](bundle-events-public-operation.md):
  language-neutral bundle event operation surface to replace any
  runtime-language-specific API.
