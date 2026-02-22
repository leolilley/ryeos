<!-- rye:signed:2026-02-22T02:31:19Z:6e1c47f51dbcc084d6809799c1214d9cd7519363ed52dbd349cfc5ec1049294a:oniwYDe9TKb3CIurXG7pKjStCOTzKzdvO2oRegRJCmeuJ5lmwVk-rRztyThOIFMbHebjNYVxUA14HGa_5FeoDw==:9fbfabe975fa5a7f -->
<!-- rye:unsigned -->
# Graph Orchestrator

Graph-specific orchestration — resume, read state, list runs, cancel, and status.

```xml
<directive name="graph_orchestrator" version="1.0.0">
  <metadata>
    <description>Graph-specific orchestration operations: resume interrupted runs, read persisted state, list graph runs, cancel running graphs, and check status. Complements the thread orchestrator — graph runs share the thread registry so wait_threads and get_status also work via the thread orchestrator.</description>
    <category>rye/agent/graphs</category>
    <author>rye-os</author>
    <model tier="general" />
    <limits max_turns="6" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.execute.tool.*</tool>
      </execute>
      <load>
        <knowledge>graphs/*</knowledge>
      </load>
      <search>
        <knowledge>*</knowledge>
      </search>
    </permissions>
  </metadata>

  <inputs>
    <input name="operation" type="string" required="true">
      Graph operation (enum: resume, read_state, list_runs, cancel, status)
    </input>
    <input name="graph_id" type="string" required="false">
      Graph tool ID (for resume, list_runs). e.g., "my-project/workflows/pipeline"
    </input>
    <input name="graph_run_id" type="string" required="false">
      Specific graph run ID (for resume, read_state, cancel, status)
    </input>
    <input name="capabilities" type="array" required="false">
      Capability strings for permission enforcement on resume
    </input>
    <input name="depth" type="integer" required="false">
      Maximum nesting depth for child thread spawns on resume (default: 5)
    </input>
  </inputs>

  <outputs>
    <output name="result">Operation result (varies by operation type)</output>
  </outputs>
</directive>
```

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
    `rye_execute(item_type="tool", item_id="{input:graph_id}", parameters={"resume": true, "graph_run_id": "{input:graph_run_id}", "capabilities": {input:capabilities}, "depth": {input:depth}})`

    **read_state** — Load the persisted state knowledge item for a graph run.
    Requires {input:graph_run_id}. The graph_id can be inferred from the run ID prefix.
    `rye_load(item_type="knowledge", item_id="graphs/<graph_id>/{input:graph_run_id}")`
    Returns the full state including frontmatter (status, current_node, step_count) and JSON body (accumulated state values).

    **list_runs** — Search for all graph run state entries, optionally filtered by graph_id.
    `rye_search(item_type="knowledge", query="entry_type:graph_state {input:graph_id}")`
    Returns a list of graph runs with their status, step count, and timestamps.

    **cancel** — Cancel a running graph by writing a cancel sentinel file.
    Requires {input:graph_run_id}.
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": ".ai/threads/{input:graph_run_id}/cancel", "content": "cancelled", "create_dirs": true})`
    The walker checks for this file after each step and terminates gracefully.

    **status** — Check the status of a graph run via the thread registry.
    Requires {input:graph_run_id}. Uses the thread orchestrator since graph runs share the same registry.
    `rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator", parameters={"operation": "get_status", "thread_id": "{input:graph_run_id}"})`
  </step>

  <step name="return_result">
    Return the operation result. Format depends on operation:
    - resume: graph execution result (graph_run_id, status, state, steps)
    - read_state: full persisted state with frontmatter and JSON body
    - list_runs: list of graph run summaries
    - cancel: confirmation that cancel sentinel was written
    - status: registry status for the graph run
  </step>
</process>

<success_criteria>
  <criterion>Operation is validated as a supported type</criterion>
  <criterion>Required parameters (graph_id, graph_run_id) are present for the chosen operation</criterion>
  <criterion>Operation executed successfully via the appropriate tool</criterion>
  <criterion>Result returned in appropriate format for the operation type</criterion>
</success_criteria>
