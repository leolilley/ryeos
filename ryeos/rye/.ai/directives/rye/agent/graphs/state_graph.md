<!-- rye:signed:2026-02-21T05:56:40Z:05960899d3f3764cfa225484650ba1e949cbd0b8fb122a332e5509cea21dc29a:uq_y3OmGyO-oeklv0zlsdtz1IwuibUXgkE8zE6bVMCvxoUQVXqNHUvdbhY94Xs-D4dGa8ghOJFVMO5OM8Q4YAA==:9fbfabe975fa5a7f -->
<!-- rye:unsigned -->
# State Graph

Execute a declarative state graph tool — walks graph YAML nodes, dispatching actions at each step.

```xml
<directive name="state_graph" version="1.0.0">
  <metadata>
    <description>Execute a state graph tool. Walks a declarative YAML graph, dispatching rye_execute for each node with state persistence and registry tracking.</description>
    <category>rye/agent/graphs</category>
    <author>rye-os</author>
    <model tier="general" />
    <limits max_turns="4" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.execute.tool.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="graph_id" type="string" required="true">
      Fully qualified graph tool ID (e.g., "my-project/workflows/code-review")
    </input>
    <input name="parameters" type="object" required="false">
      Input parameters for the graph (validated against the graph's config_schema)
    </input>
    <input name="async_exec" type="boolean" required="false">
      Run asynchronously (default: false). When true, returns immediately with graph_run_id.
    </input>
    <input name="resume_run_id" type="string" required="false">
      Resume a previous graph run from its last persisted state. Pass the graph_run_id of the run to resume.
    </input>
    <input name="capabilities" type="array" required="false">
      Capability strings for permission enforcement (e.g., ["rye.execute.tool.*"]). If omitted and running inside a thread, inherited from parent thread context.
    </input>
    <input name="depth" type="integer" required="false">
      Maximum nesting depth for child thread spawns (default: 5)
    </input>
  </inputs>

  <outputs>
    <output name="graph_run_id">Unique identifier for the graph run</output>
    <output name="status">Run status (completed, running, error, cancelled)</output>
    <output name="state">Final graph state (all accumulated assign values)</output>
    <output name="steps">Number of steps executed</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_input">
    Validate that {input:graph_id} is non-empty and well-formed.
    If empty, halt with an error.
  </step>

  <step name="build_parameters">
    Build the execution parameters dict. Start with {input:parameters} (or empty object).
    If {input:async_exec} is true, add `async_exec: true`.
    If {input:resume_run_id} is provided, add `resume: true` and `graph_run_id: "{input:resume_run_id}"`.
    If {input:capabilities} is provided, add `capabilities: {input:capabilities}`.
    If {input:depth} is provided, add `depth: {input:depth}`.
  </step>

  <step name="execute_graph">
    Execute the graph tool:
    `rye_execute(item_type="tool", item_id="{input:graph_id}", parameters=<built parameters>)`

    The graph runtime walks nodes, dispatches actions, persists state after each step, and registers the run in the thread registry.
  </step>

  <step name="return_result">
    Return the graph run result containing graph_run_id, status, state, and steps.
    If async_exec was true, return immediately with graph_run_id, status "running", and pid.
    If resuming, state continues from where the previous run left off.
  </step>
</process>

<success_criteria>
  <criterion>Graph tool ID is validated as non-empty</criterion>
  <criterion>Parameters correctly assembled including async/resume/capabilities flags</criterion>
  <criterion>Graph tool executed via rye_execute</criterion>
  <criterion>Result includes graph_run_id, status, state, and steps</criterion>
</success_criteria>
