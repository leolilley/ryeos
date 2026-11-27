<!-- ryeos:signed:2026-11-27T23:17:01Z:6d169fd0d51e3b9d61c164ae7d95808537ced471f580b09ab3807a78f53aea03:8um45RT3ecFEmPoK4mjtKCTO5OYkHCmxpfM6XMfx/QhVBFTN7kMx9LN3F5TLPq0rizpe9ex8zDvYjAJvxZUrCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/directives
tags: [directive, authoring, frontmatter]
version: "1.2.0"
description: How to author directive markdown files.
---

# Directive Authoring

Invariant: a directive file is signed markdown whose YAML metadata is composed and whose body is the prompt executed by directive-runtime.

## Design the work first

Name the judgment or workflow to perform, its inputs, bounded operations and
expected result. A directive is executable intent for a model, not a required
persona or persistent identity. Optional role instructions do not grant
authority. See [Directives](directives.md) for the conceptual distinction.

Keep required checks and disposition explicit in the surrounding workflow when
the project needs independent acceptance. A model declaring success is not a
substitute for those checks. This does not prevent a directive from using tools
or pursuing an exploratory task within its admitted limits.

## Minimal shape

The body is free-form prompt text. Markdown headings, prose and lists are
ordinary authoring choices; XML wrappers and role/task section ordering are
not required. Prompt formatting is not a mechanically enforced workflow.

```markdown
---
category: my/project
description: Do one specific job.
requires:
  capabilities:
    declared:
      - ryeos.execute.tool.echo
---

Instructions for the runtime.
```

## Important fields

- `extends`: parent directive ref. Children inherit through `extends-chain`.
- `requires.capabilities.declared`: a flat list of the capability strings the directive needs (the cap encodes its own verb, e.g. `ryeos.execute.tool.echo`). Declaring is asking, not granting: the caller launching the directive must hold every declared cap, otherwise the launch is refused with `caller_capability_not_covered`. Declare the narrowest set the body really uses; a broad wildcard can only be launched by a caller holding it. Children may narrow but not widen the parent's declared set.
- `requires.capabilities.manifest.runtime_authority`: runtime callback authority (bundle events / vault / item authoring / project snapshots / bundle state home) the daemon mints only as the signed bundle manifest backs it — not self-grantable.
- `context`: knowledge refs grouped by position (`system`, `before`, `after`).
- `model`: optional explicit provider/model/context window; otherwise routing tiers apply.
- `limits`: runtime limits such as turn/tool/token/spend budgets.
  `provider_request_body_bytes` is an exact per-model-attempt ceiling on the
  serialized provider request. `0` is unlimited. A bounded request is prepared
  once and refused before ledger admission or provider contact if it exceeds
  the signed value; RyeOS does not truncate or rewrite the transcript to fit.
  `wrap_up_seconds` and `deadline.reserve_seconds` shape the run deadline a
  caller sets with `run_timeout`; see the directives reference. Size the
  reserve from the worst-case settle time on a loaded node, not the idle case: a
  reserve that is too short (5 s on a saturated node) lets the node kill the
  runtime before it settles itself, which records a plain `timed_out` without
  the deadline phase and stop time. The PLE directives use 30.
  Directive-specific numeric dimensions are declared by the signed directive
  runtime descriptor. The engine/executor transport and clamp them as opaque
  keys and do not assign directive semantics to their names.
- `inputs` / `outputs`: structured contract for callers and summaries.
- `actions`: tool or service actions the runtime may call through callbacks.
- `execution.tool_concurrency` (default 4, range 1..=16): how many tool calls
  from ONE assistant message dispatch concurrently. Results always fold back
  in call order — the provider transcript is identical to a serial run — while
  the braid records the real shape (a batch's `tool_call_start` intents first,
  then results; consumers pair by `call_id`, never adjacency). `1` is strict
  serial dispatch. Batches carrying `directive_return` always run serially.
  Each in-flight dispatch holds one dedicated daemon connection for the
  child's whole duration, so raise the bound with the node's connection
  budget and concurrent-directive count in mind.
- `execution.observing_hooks_per_turn` (default 64, range 1..=1024): the most
  dispatches the observing hook events (`before_tool`, `after_tool`,
  `turn_incomplete`, `idle`) may make in one provider turn. Past it the rest
  are skipped and warned about once; the run is not affected.

- `provider_contact` (`once` default, or `repeatable`): your declaration that
  issuing this directive's model calls twice has no effect beyond the charge
  (no provider-hosted tools, no server-side state). Only a `repeatable`
  directive on a remote provider route may retry a model call after a failure
  that leaves it unknown whether the provider acted (timeout, dropped stream),
  and only as far as the signed execution config's
  `unknown_outcome_retry.max_retries` allows. Each such retry is charged the
  full reserved maximum. It is separate from `effects`, which concerns record
  and replay of the answer.

What the runtime enforces is its limits and the checks a directive signs;
prose instructions such as "call the tool once" are advice, and a model may
ignore them. Set `limits.tool_calls` (and `limits.turns`) to bound a run. A
`return_nudge` (`true`, or a custom message) is advice too: it gives one
corrective turn and then the run still completes. To hold a model to a method,
declare a `finish_check` hook (see the hooks section of the directives guide):
a reply with no tool call then ends the run only when the check accepts it, and
a check that cannot be satisfied ends the run failed. A check that reads files
the model itself wrote can be satisfied by files the model wrote, so a check
must read something the model cannot produce by saying so.

Keep directives focused: one job, clear inputs, explicit declared capabilities, and no hidden reliance on project-root provider configs unless trust policy allows it.

> Not to be confused with **runtime item authoring** — how an executing runtime
> proposes a new signed project item through the daemon `runtime.author_item`
> callback. That is a separate capability; see `ryeos/standard/item-authoring`.
