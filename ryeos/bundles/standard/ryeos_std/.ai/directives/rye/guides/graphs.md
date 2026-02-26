<!-- rye:signed:2026-02-26T03:49:32Z:e01bf80300ac7d614551eb19e0ade1c0f8cdafde41156073133018bc5df2fe28:0BR8iJU6ovqjE2TEa8og7xdb11w3l8kgsltmTo_hn5WUyw5_I_Nz2vELaVix9QBIrOCS_zWv3Atd6AcFK63YBw==:9fbfabe975fa5a7f -->
# Graphs

Guide 8 in the Rye OS onboarding sequence. Declarative state graphs — nodes, edges, conditions, persistence, and error handling.

```xml
<directive name="graphs" version="1.0.0">
  <metadata>
    <description>Guide 8 in the Rye OS onboarding sequence. Declarative state graphs — nodes, edges, conditions, persistence, and error handling.</description>
    <category>rye/guides</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="15" tokens="8192" />
    <permissions>
      <execute>
        <tool>rye.core.runtimes.*</tool>
        <tool>rye.agent.graphs.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="understanding">Understanding of state graphs — structure, execution, foreach, persistence, error handling</output>
    <output name="graduation">Completion of the Rye OS onboarding sequence</output>
  </outputs>
</directive>
```

<process>
  <step name="intro">
    <render>
Directives are instructions. Threads are execution. Graphs are structure.

A state graph is a YAML file that defines a workflow as nodes and edges. Each node is an action — execute a tool, run a directive, spawn a thread. Edges define transitions, optionally conditional. State flows through the graph, persisted after every step.

Instead of writing orchestration logic in Python, you declare the graph. Rye walks it.
</render>
    <instruction>Output the render block above exactly. Then proceed to the next step without waiting for user input.</instruction>
  </step>

  <step name="creation">
    <render>
Create a graph with the authoring directive:
</render>
    <tool_call>rye_execute(item_type="directive", item_id="rye/agent/graphs/create_graph")</tool_call>
    <render>
A graph is a tool — it lives in `.ai/tools/` — with `tool_type: state_graph` and `executor_id: rye/core/runtimes/state_graph_runtime`. The graph walker runtime handles the rest.
</render>
    <instruction>Output both render blocks and the tool_call example above. Then proceed to the next step.</instruction>
  </step>

  <step name="structure">
    <render>
The anatomy of a graph YAML:

```yaml
# rye:signed:TIMESTAMP:HASH:SIG:FP
tool_type: state_graph
executor_id: rye/core/runtimes/state_graph_runtime

initial_node: start

nodes:
  start:
    action:
      primary: execute
      item_type: tool
      item_id: my-tool
      parameters:
        input: "{state.input}"
    assign:
      result: "{action_result.output}"
    edges:
      - condition: "{state.result}"
        target: process
      - target: error_handler

  process:
    action:
      primary: execute
      item_type: tool
      item_id: process-tool
      parameters:
        data: "{state.result}"
    assign:
      final: "{action_result.data}"
    edges:
      - target: done

  error_handler:
    action:
      primary: execute
      item_type: tool
      item_id: notify-error
    on_error: done
    edges:
      - target: done

  done:
    type: return
```

Key elements:
- **initial_node** — where execution begins
- **action** — what the node does (uses the same `primary`/`item_type`/`item_id` as rye_execute)
- **assign** — write action results into state variables
- **edges** — where to go next (optional conditions using `{state.var}` interpolation)
- **type: return** — terminal node, graph execution ends here
</render>
    <instruction>Output the render block above exactly. Then proceed to the next step.</instruction>
  </step>

  <step name="execution">
    <render>
Execute a graph like any other tool:
</render>
    <tool_call>rye_execute(item_type="tool", item_id="my-project/my-graph", parameters={"input": "hello"})</tool_call>
    <render>
The `state_graph_walker` takes over:

1. Load graph YAML, parse nodes and edges
2. Start at `initial_node`
3. Dispatch the node's action through rye_execute
4. Apply `assign` — write results to state
5. Evaluate edges — first matching condition wins
6. Move to next node
7. Repeat until a `return` node or dead end
8. Persist state after every step

Every action dispatch goes through the same executor chain — signed, verified, capability-checked. The graph walker doesn't bypass anything.
</render>
    <instruction>Output both render blocks and the tool_call example above. Then proceed to the next step.</instruction>
  </step>

  <step name="foreach">
    <render>
Foreach nodes handle parallel fan-out:

```yaml
fan_out:
  type: foreach
  collection: "{state.items}"
  item_var: current_item
  body_node: process_item
  gather_into: results
  edges:
    - target: aggregate
```

This iterates over `state.items`, runs the `process_item` node for each element (with `current_item` set in state), and gathers all results into `state.results`. Then transitions to `aggregate`.

Foreach is how you process lists — scrape N websites, score N leads, generate N reports — without writing loop logic.
</render>
    <instruction>Output the render block above exactly. Then proceed to the next step.</instruction>
  </step>

  <step name="persistence">
    <render>
State is persistent. After every step, the graph walker writes state as a signed knowledge item:

```
.ai/knowledge/graphs/{graph_id}/{run_id}/state.yaml
```

This means:
- **Crash recovery** — if the graph fails or is interrupted, it resumes from the last persisted state
- **Auditability** — every intermediate state is signed and timestamped
- **Debugging** — inspect state at any step to see what happened

State includes: current node, all state variables, step count, error history, timestamps.
</render>
    <instruction>Output the render block above exactly. Then proceed to the next step.</instruction>
  </step>

  <step name="error_handling">
    <render>
Error handling is declarative:

- **`on_error: target_node`** — route errors to a recovery node
- **`error_mode: "fail"`** — terminate the graph on error (default)
- **`error_mode: "continue"`** — skip the failed assignment, keep going
- **Error classification** — `error_loader.classify()` categorizes errors automatically
- **Hooks** — `"error"` hooks fire first, can return retry instructions

```yaml
risky_node:
  action:
    primary: execute
    item_type: tool
    item_id: unreliable-api
  on_error: fallback_node
  error_mode: continue
  edges:
    - target: next_step

fallback_node:
  action:
    primary: execute
    item_type: tool
    item_id: notify-failure
  edges:
    - target: next_step
```

Errors don't have to be fatal. Route them, retry them, continue past them — it's all in the graph definition.
</render>
    <instruction>Output the render block above exactly. Then proceed to the next step.</instruction>
  </step>

  <step name="graduation">
    <instruction>Output the render block below EXACTLY and in FULL. Do not truncate, summarize, or rephrase.</instruction>
    <render>
Congratulations — you've now covered everything in the base Rye OS package.

Directives, knowledge, tools. Three-tier spaces. Signing and verification. Core utilities. The registry. Multi-file tools and bundles. MCP integration. Threading and orchestration. Declarative state graphs.

Now it's time to start building your own. Need inspiration? Instruct me to search the registry for Rye's public demos:

```
rye search directive "demo" --space registry
```

Available demos:
- Lead generation pipeline (multi-phase orchestration)
- Code review automation (threaded analysis)
- Knowledge graph builder (graph + threading)
- Self-improving workflow (directive self-modification)
- MCP bridge (connect external tools)

Happy building.
</render>
  </step>
</process>

<success_criteria>
<criterion>User understands state graph structure — nodes, edges, actions, assign, conditions</criterion>
<criterion>User understands graph execution flow via state_graph_walker</criterion>
<criterion>User understands foreach fan-out for parallel processing</criterion>
<criterion>User understands state persistence and crash recovery</criterion>
<criterion>User understands declarative error handling</criterion>
<criterion>User has graduated from the Rye OS onboarding sequence</criterion>
</success_criteria>
