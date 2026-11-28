<!-- ryeos:signed:2026-11-28T20:01:13Z:7387b30b943f03ca8cbffe32892e5b120956cc6f3facf7fd2280cfd6bb1077cb:7iSB2As+rI6Py7i+d22fLoPygvPJlhPIvErah+2Th+8caJ2XbMsNApBbHJOSOWTy29o5lBSaJhN1aiQLiRgZBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/graphs
tags: [graph, authoring, dag, workflow]
version: "1.1.0"
description: How to author graph workflow YAML.
---

# Graph Authoring

Invariant: a graph is signed YAML describing explicit nodes, edges, conditions, and the capabilities it needs for daemon-mediated action callbacks.

## Authoring checklist

- Declare `category`, `version`, and a clear description.
- Declare `requires.capabilities.declared` (a flat list of caps) for every daemon action callback the graph may perform. The caller launching the graph must hold every declared cap (`caller_capability_not_covered` otherwise); declaring never grants authority.
- Keep node ids stable; persisted state and events refer to them.
- Use conditional edges for branching and foreach blocks for fan-out.
- Prefer explicit error edges/hooks over relying on runtime defaults.
- Keep side-effecting nodes isolated so resume/retry behavior is understandable.
- When an action proposes or its node authors `project_observations`, give each
  claim a bounded namespaced `namespace`, a source-owned stable `stable_id`,
  and a bounded meaning-blind `payload`. RyeOS publishes it at the graph commit
  boundary; callers cannot assert the graph source identity. Prefer the
  node-authored form when the accepted action result already contains every
  claimed value; reserve hooks for observers that actually execute.

The graph kind delegates to `runtime:graph-runtime` through the runtime registry.

## Example: a goal, a working directive and a judge

`graph:ryeos/standard/examples/goal-judge` (in the directive bundle, with `directive:ryeos/standard/examples/goal-work`, `directive:ryeos/standard/examples/goal-judge`, `tool:ryeos/standard/examples/goal-repair` and `tool:ryeos/standard/examples/goal-fail`) is a signed example, not a default: no shipped graph, directive or allowlist refers to it. Copy it and change the signed state at the top of the graph.

What it does, each stage a followed child with a fresh context:

1. The working directive is given the goal, the round number and, after a rejection, the author's repair text. It returns one `deliverable`.
2. Deterministic gates (graph expressions over the deliverable, here its length) run first. A gate failure is final for the round: the judge is not asked, so it can never override a gate. The working directive is re-run with the signed repair text for the gate.
3. A judge directive with no tool and no capability is given the goal, the signed criteria and the deliverable as one JSON-quoted string in a fixed frame. Nothing of the working directive's transcript, notes or claims reaches it. It returns a closed result: `pass` and `failed`, the keys of the signed criteria not met.
4. A result outside that shape ends the graph failed with `judge_result_invalid`: a wrong type, or a key that is not a signed criterion (the signed repair tool refuses it, in every round including the last). A judge's own words never reach the working directive; it is only ever given the author's signed repair text for the failed keys.
5. `max_rounds` is signed state. A start gate ends the graph failed with `goal_config_invalid` before any directive runs unless it is a whole number from 1 to 8 (2.5, 0 and 9 are refused). After `max_rounds` rounds the graph ends failed with `goal_rounds_exhausted`, never successful. Rounds count against the graph's own limits (here `max_steps`; the time and spend limits of the launch apply as they do to any graph), and a limit's own failure wins: a graph stopped by `max_steps` fails with `exceeded max_steps`, not with the example's code, and never completes.
6. The graph returns the working and judge thread ids of every round in `trail` (`work:<round>=<thread>;`, `judge:<round>=<thread>;`). Each of those threads records the model it ran on in its own `cognition_frame` event (`request_options.model`) beside the hash of the provider configuration that route resolved to, so a reader tells work from judge by the thread id. The judge may use the same route as the working directive; that is recorded, not forbidden.

The example directives declare turn and token limits but no `spend_usd`: a hard spend limit is refused at launch (`hard_spend_bound_ineligible`) on a route whose spend is only advisory, so add one to your copy when your route is priced.

The three failure codes come from a signed tool that exits non-zero, so the run's failure carries the code in its diagnostic. A graph cannot yet end failed with its own code from a return node; that is a known gap (`fail: {code, message}` on the return node, Q-19).

**What this does not protect against.** A judge model can be talked into passing. The deliverable is text the working directive wrote, and it may carry instructions aimed at the judge. Quoted data, no tools, a closed output, signed repair text and gates that run first reduce what a talked-over judge can do; they do not remove the risk. The pattern is not a default until a measurement shows it moves a result.
