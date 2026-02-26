<!-- rye:signed:2026-02-25T07:50:41Z:24e41a60be6175dbfc1e6eafb6bbbf40278305aa02d6c7733c1b7cfa6a50ec9e:7HJl09yAzIthB4nTEsxlwQ49b6L0BpiGsFkfEiYPeEKDBsXbg-rFHSvak-Rpd4IlT_Q4C0HGtONMXORl1kI6Dw==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:cc86d4f73becccd24d3dba9ad3ba65595f7292445bc7250c2eb80b1262ffb2db:0NFp80QZvi71RAUfaXUwHwD2gbozWoxsUO5U_wtnd54w6nK3HCdtBVAJX-tDm9HwWaBr3QOCw7A5da0ph_AfBg==:9fbfabe975fa5a7f -->
<!-- rye:unsigned -->

# Create Graph Tool

Create a declarative state graph tool — a YAML workflow definition with nodes, edges, and state management.

```xml
<directive name="create_graph" version="1.0.0">
  <metadata>
    <description>Creates a state graph tool YAML with nodes, edges, assign expressions, and optional hooks. The graph is executed by state_graph_runtime.</description>
    <category>rye/agent/graphs</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="8" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>
        <tool>*</tool>
      </search>
      <load>
        <tool>*</tool>
        <knowledge>*</knowledge>
      </load>
      <sign>
        <tool>*</tool>
      </sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="name" type="string" required="true">
      Name of the graph tool (snake_case, e.g., "code_review_pipeline")
    </input>
    <input name="category" type="string" required="true">
      Category path (e.g., "workflows/code-review", "my-project/pipelines")
    </input>
    <input name="description" type="string" required="true">
      What the graph workflow does
    </input>
    <input name="nodes" type="string" required="true">
      Comma-separated list of node names in execution order (e.g., "analyze,fix,review,report")
    </input>
    <input name="input_fields" type="string" required="false">
      Comma-separated input field names for config_schema (e.g., "files,threshold")
    </input>
    <input name="max_steps" type="integer" required="false">
      Maximum steps before graph terminates (default: 50)
    </input>
    <input name="error_mode" type="string" required="false">
      Error handling policy: "fail" (default) or "continue"
    </input>
  </inputs>

  <outputs>
    <output name="tool_path">Path to the created graph tool YAML file</output>
    <output name="signed">Whether the tool was successfully signed</output>
  </outputs>
</directive>
```

<process>
  <step name="load_reference">
    Load the state graph walker knowledge entry for structural reference:
    `rye_load(item_type="knowledge", item_id="rye/core/runtimes/state-graph-walker")`

    Also search for existing graph tools to avoid duplication:
    `rye_search(item_type="tool", query="{input:name} graph")`
  </step>

  <step name="build_graph_yaml">
    Generate the graph tool YAML file with this structure:

    ```yaml
    version: "1.0.0"
    tool_type: graph
    executor_id: rye/core/runtimes/state_graph_runtime
    category: {input:category}
    description: "{input:description}"

    config_schema:
      type: object
      properties:
        # One entry per {input:input_fields} field
      required: [...]

    config:
      start: <first node from {input:nodes}>
      max_steps: {input:max_steps}
      on_error: {input:error_mode}

      nodes:
        <first_node>:
          action:
            primary: execute
            item_type: tool
            item_id: <placeholder — agent fills in>
            params: {}
          assign:
            <key>: "${result.<field>}"
          next: <second_node>

        # ... one node per entry in {input:nodes} ...

        <last_node>:
          type: return
    ```

    Rules:
    - Parse {input:nodes} into ordered node names
    - First node is the `start` node
    - Last node should be `type: return` unless it has an action
    - Each intermediate node gets a skeleton action (primary: execute), empty assign, and next pointing to the following node
    - Use `${inputs.*}` for input references, `${state.*}` for state, `${result.*}` for action output
    - Add `on_error` only if {input:error_mode} is provided
  </step>

  <step name="write_tool">
    Write the graph YAML to .ai/tools/{input:category}/{input:name}.yaml:
    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": ".ai/tools/{input:category}/{input:name}.yaml", "content": "<generated YAML>", "create_dirs": true})`
  </step>

  <step name="sign_tool">
    Sign the new graph tool:
    `rye_sign(item_type="tool", item_id="{input:category}/{input:name}")`
  </step>
</process>

<success_criteria>
  <criterion>No duplicate graph tool with the same name exists</criterion>
  <criterion>Graph YAML has executor_id: rye/core/runtimes/state_graph_runtime</criterion>
  <criterion>All nodes from {input:nodes} are present with correct next edges</criterion>
  <criterion>Last node is type: return or has a terminal edge</criterion>
  <criterion>config_schema includes fields from {input:input_fields}</criterion>
  <criterion>File created at .ai/tools/{input:category}/{input:name}.yaml</criterion>
  <criterion>Signature validation passed</criterion>
</success_criteria>

