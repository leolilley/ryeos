---
description: "Graph-specific orchestration operations: resume interrupted runs, read persisted state, list graph runs, cancel running graphs, and check status. Complements the thread orchestrator — graph runs share the thread registry so wait_threads and get_status also work via the thread orchestrator."
version: "1.0.0"
model_tier: general
limits:
  turns: 6
  tokens: 4096
permissions:
  execute:
    - tool:rye.execute.tool.*
  fetch:
    - knowledge:graphs/*
    - knowledge:*
---

# Graph Orchestrator

Graph-specific orchestration — resume, read state, list runs, cancel, and status.

<process>
  <step name="validate_operation">
    Validate that {input:operation} is one of the supported operations:
    resume, read_state, list_runs, cancel, status.
    If invalid, halt with an error listing valid operations.
  </step>

  <step name="execute_operation">
    Execute based on {input:operation}:

    **resume** — Resume a failed or interrupted graph run from its last persisted state.
    Requires {input:graph_id} and {input:graph_run_id}.
    `rye_execute(item_id="{input:graph_id}", parameters={"resume": true, "graph_run_id": "{input:graph_run_id}", "capabilities": {input:capabilities}, "depth": {input:depth}})`

    **read_state** — Load the persisted state knowledge item for a graph run.
    Requires {input:graph_run_id}. The graph_id can be inferred from the run ID prefix.
    `rye_fetch(item_type="knowledge", item_id="graphs/<graph_id>/{input:graph_run_id}")`
    Returns the full state including frontmatter (status, current_node, step_count) and JSON body (accumulated state values).

    **list_runs** — Search for all graph run state entries, optionally filtered by graph_id.
    `rye_fetch(scope="knowledge", query="entry_type:graph_state {input:graph_id}")`
    Returns a list of graph runs with their status, step count, and timestamps.

    **cancel** — Cancel a running graph via SIGTERM signal.
    Requires {input:graph_run_id}.
    `rye_execute(item_id="rye/core/processes/cancel", parameters={"run_id": "{input:graph_run_id}"})`
    The walker's SIGTERM handler triggers clean shutdown — persists CAS state, updates registry, writes transcript event.

    **status** — Check the status and liveness of a graph run.
    Requires {input:graph_run_id}. Uses the process status tool which checks both registry status and PID liveness.
    `rye_execute(item_id="rye/core/processes/status", parameters={"run_id": "{input:graph_run_id}"})`
  </step>

  <step name="return_result">
    Return the operation result. Format depends on operation:
    - resume: graph execution result (graph_run_id, status, state, steps)
    - read_state: full persisted state with frontmatter and JSON body
    - list_runs: list of graph run summaries
    - cancel: confirmation that SIGTERM was sent and process terminated
    - status: registry status and PID liveness for the graph run
  </step>
</process>

<success_criteria>
  <criterion>Operation is validated as a supported type</criterion>
  <criterion>Required parameters (graph_id, graph_run_id) are present for the chosen operation</criterion>
  <criterion>Operation executed successfully via the appropriate tool</criterion>
  <criterion>Result returned in appropriate format for the operation type</criterion>
</success_criteria>
