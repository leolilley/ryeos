<!-- ryeos:signed:2026-11-18T07:27:10Z:994cde2d18df6ab6d51475b7bba4d277fe1bd8d2935d74eb29baeb2d0be77267:RnBymX4LD0Dbr+DhTcXX5sn79wnYKb5AKHrdy5zc0GM5iGjU1lqkrHZQvGjy04L6iCGEAewnryOPdeaZAXTbBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [fundamentals, directives, workflows, prompts]
version: "2.1.0"
description: >
  How directives work — YAML frontmatter, free-form prompt body,
  inheritance (extends), capability requirements, limits, and context blocks.
---

# Directives

**A directive authors executable intent for a model—not necessarily an
enduring assistant persona.** It is the primary model-driven item in RyeOS:
authored instructions, composed context, capability requirements and limits.
A markdown file carries the definition; an admitted invocation performs it.

## Why a directive, rather than an agent?

Start by asking what work requires judgment, what inputs it receives and what
it may do. You do not first need to invent a persistent character, workspace
owner or independent identity.

A directive may classify one observation, propose an experiment, or conduct a
longer tool-using investigation within its limits. The runtime's model/tool
loop overlaps with what other systems call an agent loop. The distinction is
what the item names: executable work, not necessarily an enduring entity.

The definition, its individual executions, the model/runtime and the principal
authorising the work are different things. A model's role description is
prompt content; it does not create a key or grant capabilities. See
[Identity model](../../core/identity-model.md).

## Compose judgment into a system

Tools perform operations, directives exercise model-driven judgment, and graphs
coordinate explicit transitions. The project defines objectives and required
evidence. A campaign can use a directive without delegating its acceptance
criteria or publication authority to that directive.

For example, a directive can propose a candidate from admitted observations;
a tool can evaluate it; a graph can require that result before a separately
authorised publication. A successful directive return establishes completion,
not that the proposal is correct or approved.

This is a design pattern, not a requirement to put every decision in a graph.
Direct execution and open-ended investigation remain valid within their
admitted contracts. Assistant experiences can also be built on this substrate.

## File Format

```markdown
---
description: "Review a proposed deployment from supplied evidence"
version: "1.0.0"
model:
  tier: high
requires:
  capabilities:
    declared: []
limits:
  turns: 10
  tokens: 8000
  spend_usd: "0.5"
  duration_seconds: 300
---

Review the proposed deployment to ${inputs.environment} using the supplied
evidence. Identify unresolved risks and missing checks. Distinguish observed
facts from assumptions, and return a concise recommendation.

This review does not authorise or perform deployment.
```

The `name` and `category` fields are NOT in the frontmatter — they are
derived automatically from the file path:
- `name` comes from the filename (e.g., `deploy.md` → name: `deploy`)
- `category` comes from the parent directory (e.g., `my-project/deploy.md` → category: `my-project`)

## Body: prompt text

The body is free-form prompt text, commonly written in Markdown. RyeOS does
not require XML, a `<process>` wrapper, an `<Identity>` section, or any ordering
between role instructions and task instructions. Authors may use headings,
lists or tags when useful, but they are prompt structure, not an execution DSL.

In particular, a `<render>` tag does not bypass model interpretation or
guarantee verbatim output. Named steps and condition attributes in prompt text
do not establish mechanically enforced ordering or branching. Use executable
workflow contracts where those guarantees are required. A prompted role does
not establish cryptographic identity or authority.

### Input interpolation

Use rye-expr/1 `${inputs.name}` expressions for input interpolation. Use
  `${inputs.name ?? "default"}` for a nullish fallback and `${json(inputs.value)}`
  when structured data must be embedded in text. Directive bodies expose only
  the `inputs` root, and each reference must name one exact input. Dynamic
  indexes such as `inputs[key]` are rejected (literal `inputs["name"]` is
  accepted) so RyeOS can append every unreferenced input exactly once. `$${`
  emits a literal `${`.

## Frontmatter Fields

### Model Selection
- `model.tier` — abstract capability tier: `fast`, `general`, `high`,
  `orchestrator`, `max`, `code`, `code_max`, `cheap`, `free`
- `model.name` — explicit model name (overrides tier)
- `model.context_window` — override context window size

### Capabilities
- `requires.capabilities.declared` — a flat list of self-asserted capability
  strings the item is allowed to invoke (the cap encodes its own verb). Uses
  dot-namespaced glob patterns:
  - `["ryeos.execute.tool.ryeos.file-system.*"]` — all FS tools
  - `["ryeos.execute.service.fetch"]` — just the fetch service
  - `[]` (or omitted) — no tool execution (read-only directive)
- `requires.capabilities.manifest` — runtime callback authority (bundle events /
  vault / item authoring / project snapshots) the daemon mints only as the
  signed bundle manifest backs it; not self-grantable.

`declared` and `manifest` inherit independently through extends chains. When a
child declares either subtree, that complete declaration replaces the inherited
subtree and must be covered by the immediately effective parent; any widening
fails composition. Omission inherits, while an explicit empty declaration
removes that subtree's authority.

### Limits
- `limits.turns` — max LLM round-trips
- `limits.tool_calls` — max non-lifecycle tool-call attempts across the run.
  Once exhausted, the directive runtime returns ordered limit results for any
  excess calls and stops advertising dispatchable tools, so the model must
  answer from results already gathered. `0` means unlimited.
- `limits.tokens` — settled provider-native token threshold (checked after
  each attempt settles; not a pre-issue token reservation)
- `limits.spend_usd` — hard USD budget as a canonical decimal string (e.g.
  `"0.5"`; numeric values are rejected). A finite value requires the route to
  carry a mechanically proven spend bound (signed tariff or provider-enforced
  charge cap): the daemon reserves each attempt's worst-case charge before any
  provider contact and shares one allowance across all paid descendants.
  Routes without a proven bound reject a finite hard limit at launch.
  Conservative reserved-maximum charges are reported distinctly from
  provider-reported cost.
- `limits.duration_seconds` — wall-clock timeout of one process. A
  continuation or a resume starts a new process with a new clock, so this is
  not a limit on the whole run; use the run deadline below for that.
- `limits.deadline.reserve_seconds` — time this directive keeps for itself to
  settle after its work is stopped (see Run deadline). Defaults to the signed
  directive limits config.
- `limits.wrap_up_seconds` — how long before the hard stop the model is told
  time is short. `0` never tells it. Only meaningful for a run with a deadline.

### Run deadline

A caller sets one deadline for a whole run by passing `run_timeout` (unsigned
seconds) when it launches the root execution. The daemon turns it into one
absolute instant when the execution tree is born. Every descendant, continuation
and resume shares that instant, and restarting the node does not restart the
clock. The node's `caps.aggregate.duration_seconds` caps what a caller may ask
for. Only the root launch may set it: a child that sends `run_timeout` is
refused with `run_timeout_root_only`.

Each level of the tree ends earlier than its parent. An execution keeps its
`limits.deadline.reserve_seconds` for itself, so a child launched before the
parent's reserve window opens is stopped when the window opens, and the parent
still has the reserve to react. A child launched inside the parent's reserve
window (the parent's own fallback work) may use what is left of it, never more.

A directive run with a deadline moves through three phases, with `R` the
reserve and `E` the execution's end:

- running, until `E - R - wrap_up_seconds`;
- wrap-up, until `E - R`: once, the model receives a harness input of kind
  `deadline_wrap_up` stating the time left. It is recorded durably and is not
  repeated after a recovery or continuation;
- hard stop, from `E - R`: any model call in flight is cut, no new turn or tool
  call starts, and the run settles `timed_out` with outcome `deadline`. The
  daemon's own kill at `E` is only the backstop.

Reserves stack with depth. Each level of the tree keeps its own reserve out of
its descendants' time, and every directive defaults to the signed 30 s reserve,
so a child ends 30 s before its parent's hard stop, a grandchild 60 s before,
and so on. A short `run_timeout` with deep nesting can leave a deep descendant
no window at all (it stops as soon as it launches); size `run_timeout` to at
least the depth times the reserve plus the work, or author smaller reserves on
the intermediate items.

A directive's tools are bounded by its hard stop: a tool still running when the
hard stop arrives is cut there, whatever timeout the tool declares. The cut call
is recorded as `tool_interrupted_at_deadline`, an interrupted call whose outcome
is unknown, because the tool may have partially acted before it was stopped. It
is never retried, and the model sees it as an error with that code. A tool call
made at or after the hard stop does not start. A tool's own timeout still
applies when it is the shorter.

The model can ask for the clock with the built-in `time_left` tool, which is
offered only when the run has a deadline. It returns `now_ms`,
`deadline_at_ms`, `remaining_ms` (until the hard stop), `phase`,
`wrap_up_at_ms` and `hard_stop_at_ms`.

A continuation, resume or follow successor whose end has already passed does
not launch: it settles `timed_out` with outcome `deadline` and phase
`elapsed_before_launch`. A thread whose deadline passes while the node is down
settles the same way when the node recovers. A graph reads the outcome as
`outcome_kind: deadline` on its failed branch.

### Context
Context blocks inject knowledge into the LLM prompt:

```yaml
context:
  - position: system
    ref: "knowledge:ryeos/core/signing"      # knowledge entry
  - position: system
    content: "Inline text content"            # literal content
  - position: user
    ref: "knowledge:project/context"          # in user position
```

Context merges through extends chains using
`dict_merge_string_seq_root_last` — the child's context entries
are appended after the parent's.

### Hooks

Directive frontmatter may declare `hooks` for `after_step` and `continuation`.
Each hook has `id`, `event`, required `result`, optional `condition`, and
`action`. `discard` and `observation` are observers; `control` is accepted only
where the signed event and layer permit directive control.

Authored hook inheritance is atomic. Omitting `hooks` inherits the nearest
ancestor's complete list. `hooks: []` clears it. A declared non-empty list
replaces it completely; hooks are not merged by ID. `hooks: null` and duplicate
effective IDs fail composition/admission.

After context rendering, the daemon captures authored hooks together with
signed configured policy into the finalized effective program. Authored hooks
use the directive's admitted caps; configured hooks use only their signed
source's grants. The directive runtime compiles that captured plan and never
loads hook policy from the live filesystem.

## Inheritance (Extends)

Directives support single inheritance via `extends`:

```yaml
extends: "directive:base/workflow"
```

The extends-chain composer resolves the full chain (root → ... → child)
and merges fields with declared strategies:

| Field          | Strategy                          |
|----------------|-----------------------------------|
| `body`         | `root_verbatim` — child replaces parent body |
| `requires`     | `narrow_requires_capabilities` — each declared subtree atomically replaces its inherited value and fails on widen |
| `context`      | `dict_merge_string_seq_root_last` — child appended |
| `model`        | nearest declaration, root last |
| `limits`       | shallow mapping merge, root last |
| `inputs`       | keyed merge by input name, root last |
| `hooks`        | nearest complete list, root last |

This means:
- A child directive always overrides the prompt body
- A child can never gain more capabilities than its parent
- Context accumulates: parent context + child context

## Execution Lifecycle

1. **Resolve** — canonical ref → file path → parsed metadata
2. **Compose** — extends chain resolved, fields merged
3. **Capture and finalize** — rendered context and the complete hook plan are
   validated, mutable dependencies are rechecked, and one
   `effective_definition_digest` is sealed
4. **Launch** — directive-runtime subprocess spawned with:
   - Composed prompt body
   - Context blocks assembled into system/user positions
   - Input values interpolated
   - Permission caps set
5. **Run** — LLM loop with tool dispatch (up to `limits.turns`)
6. **Complete** — result captured, thread finalized
