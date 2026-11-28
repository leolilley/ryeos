<!-- ryeos:signed:2026-11-28T14:17:16Z:c42458174e8a5c999cc98d4c880f6acd0848f50a68e30fccf176d3a3d76d936e:ryH6yPevoLGkrRhoXGIqMFEB/TwMuyJYpIhqS3RnpeCOODx5ug2c17YhBAOrtuMB6MDUIBLCz3kBb4bc+qUiDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

`graph:ryeos/standard/examples/goal-judge` (in the graph bundle, with `directive:ryeos/standard/examples/goal-work`, `directive:ryeos/standard/examples/goal-judge` and `tool:ryeos/standard/examples/goal-fail`) is a signed example, not a default: no shipped graph, directive or allowlist refers to it. Copy it and change the signed state at the top of the graph.

What it does, each stage a followed child with a fresh context:

1. The working directive is given the goal, the round number and, after a rejection, the author's repair text. It returns one `deliverable`.
2. Deterministic gates (graph expressions over the deliverable, here its length) run first. A gate failure is final for the round: the judge is not asked, so it can never override a gate. The working directive is re-run with the signed repair text for the gate.
3. A judge directive with no tool and no capability is given the goal, the signed criteria and the deliverable as one JSON-quoted string in a fixed frame. Nothing of the working directive's transcript, notes or claims reaches it. It returns a closed result: `pass` and `failed`, the keys of the signed criteria not met.
4. A result outside that shape (a wrong type, or a key that is not a signed criterion) ends the graph failed with `judge_result_invalid`. A judge's own words never reach the working directive; it is only ever given the author's signed repair text for the failed keys.
5. After `max_rounds` (signed state, 1 to 8) the graph ends failed with `goal_rounds_exhausted`, never successful. Rounds count against the graph's turn, time and spend limits; a limit's own failure code wins.
6. The graph records the working and judge thread ids of every round in its result (`trail`); each thread's own record holds the route and model it ran on. The judge may use the same route as the working directive; that is recorded, not forbidden.

The two failure codes come from a signed tool that exits non-zero, so the run's failure carries the code in its diagnostic. A graph cannot yet end failed with its own code from a return node; that is a known gap (`fail: {code, message}` on the return node, Q-19).

**What this does not protect against.** A judge model can be talked into passing. The deliverable is text the working directive wrote, and it may carry instructions aimed at the judge. Quoted data, no tools, a closed output, signed repair text and gates that run first reduce what a talked-over judge can do; they do not remove the risk. The pattern is not a default until a measurement shows it moves a result.
