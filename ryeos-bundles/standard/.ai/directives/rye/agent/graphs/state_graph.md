<!-- ryeos:signed:2026-05-17T21:44:36Z:f293a0fa244035b49924e0d796c7b230050affbcf0ff128ff47423a9c50e20e9:1DHh4q9ld63DlKqh8gSFrvcTmhEovqej//HNoCMPuXRSXVdocUaYbMw+ej8COsTY3ITIgHTyU+M45v33v1OXCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Execute a state graph tool. Walks a declarative YAML graph, dispatching rye_execute for each node with state persistence and registry tracking."
version: "1.0.0"
model_tier: general
limits:
  turns: 4
  tokens: 4096
permissions:
  execute:
    - tool:rye.execute.tool.*
---

# State Graph

Execute a declarative state graph tool — walks graph YAML nodes, dispatching actions at each step.

<process>
  <step name="validate_input">
    Validate that {input:graph_id} is non-empty and well-formed.
    If empty, halt with an error.
  </step>

  <step name="build_parameters">
    Build the execution parameters dict. Start with {input:parameters} (or empty object).
    If {input:async} is true, add `async: true`.
    If {input:resume_run_id} is provided, add `resume: true` and `graph_run_id: "{input:resume_run_id}"`.
    If {input:capabilities} is provided, add `capabilities: {input:capabilities}`.
    If {input:depth} is provided, add `depth: {input:depth}`.
  </step>

  <step name="execute_graph">
    Execute the graph tool:
    `rye_execute(item_id="{input:graph_id}", parameters=<built parameters>)`

    The graph runtime walks nodes, dispatches actions, persists state after each step, and registers the run in the thread registry.
  </step>

  <step name="return_result">
    Return the graph run result containing graph_run_id, status, state, and steps.
    If async was true, return immediately with graph_run_id, status "running", and pid.
    If resuming, state continues from where the previous run left off.
  </step>
</process>

<success_criteria>
  <criterion>Graph tool ID is validated as non-empty</criterion>
  <criterion>Parameters correctly assembled including async/resume/capabilities flags</criterion>
  <criterion>Graph tool executed via rye_execute</criterion>
  <criterion>Result includes graph_run_id, status, state, and steps</criterion>
</success_criteria>
