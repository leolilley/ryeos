<!-- rye:signed:2026-03-29T06:38:41Z:f09f243cd34dec1ad6ee9da05fed961f7b124d810b41915bb5191ccbf59ca7f7:Axo_AzxyJUNomM4dJcn3qNuNlU2qPBBSxmtmrLs0WIEULZ-AUM5nO8BVNUVE8AsdsZH-nMOzeWLR-XFtH4hyAw==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

# Manage Processes

Manage running processes — check status, cancel, list, and resume graphs and threads.

```xml
<directive name="manage" version="1.0.0">
  <metadata>
    <description>Manage running processes (graphs and threads): check status, cancel with graceful SIGTERM, list active or filtered processes, and resume cancelled graphs from CAS state.</description>
    <category>rye/core/processes</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="6" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.execute.tool.rye.core.processes.*</tool>
        <tool>rye.execute.tool.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="operation" type="string" required="true">
      Process operation (enum: status, cancel, list, resume)
    </input>
    <input name="run_id" type="string" required="false">
      graph_run_id or thread_id for status, cancel, and resume operations
    </input>
    <input name="graph_id" type="string" required="false">
      Graph tool ID for resume (e.g., "my-project/workflows/pipeline")
    </input>
    <input name="status_filter" type="string" required="false">
      Filter for list operation (e.g., "running", "cancelled", "completed")
    </input>
    <input name="grace" type="number" required="false">
      Grace period in seconds for cancel operation (default: 5)
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
    status, cancel, list, resume.
    If invalid, halt with an error listing valid operations.
  </step>

  <step name="execute_operation">
    Execute based on {input:operation}:

    **status** — Check the status of a running process.
    Requires {input:run_id}.
    `rye_execute(item_type="tool", item_id="rye/core/processes/status", parameters={"run_id": "{input:run_id}"})`

    **cancel** — Cancel a running process via SIGTERM-based graceful shutdown.
    Requires {input:run_id}. Optional {input:grace} (default: 5).
    `rye_execute(item_type="tool", item_id="rye/core/processes/cancel", parameters={"run_id": "{input:run_id}", "grace": {input:grace}})`

    **list** — List active or filtered processes.
    Optional {input:status_filter}.
    `rye_execute(item_type="tool", item_id="rye/core/processes/list", parameters={"status_filter": "{input:status_filter}"})`

    **resume** — Resume a cancelled graph from its last persisted CAS state.
    Requires {input:graph_id} and {input:run_id}.
    `rye_execute(item_type="tool", item_id="{input:graph_id}", parameters={"resume": true, "graph_run_id": "{input:run_id}"})`
  </step>

  <step name="return_result">
    Return the operation result. Format depends on operation:
    - status: process status (run_id, state, timestamps)
    - cancel: confirmation of SIGTERM delivery and graceful shutdown
    - list: list of processes matching the filter criteria
    - resume: graph execution result (graph_run_id, status, state, steps)
  </step>
</process>

<success_criteria>
  <criterion>Operation is validated as a supported type</criterion>
  <criterion>Required parameters (run_id, graph_id) are present for the chosen operation</criterion>
  <criterion>Operation executed successfully via the appropriate process tool</criterion>
  <criterion>Result returned in appropriate format for the operation type</criterion>
</success_criteria>
