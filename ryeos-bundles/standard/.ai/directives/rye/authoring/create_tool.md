<!-- ryeos:signed:2026-05-17T21:44:36Z:e25b2d679e87ebb28a3576f72bf4e69427a4ddfde1eb752c7b9bddd9759975e5:9JcK/IOLu5jaSHY+Q6EdEC/iNTtt90jZLT/YAFdBYY/O1rNvptifjhkz+nlppV5+v6k3vybEJc7ZaL6k0CKFAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Create a tool file with correct metadata headers and parameter schema, then sign it."
version: "3.0.0"
model_tier: fast
limits:
  turns: 8
  tokens: 4096
permissions:
  execute:
    - tool:rye.file-system.*
  fetch:
    - tool:*
  sign:
    - tool:*
---

# Create Tool

Create a new tool (Python, YAML, etc.) with proper metadata, validate, and sign it.

<process>
  <step name="check_existing">
    Search for existing tools with a similar name to avoid duplicates.
    `rye_fetch(scope="tool", query="{input:tool_name}")`
  </step>

  <step name="write_tool_file">
    Determine the file extension from {input:tool_type}: python=.py, yaml=.yaml, bash=.sh, javascript=.js

    Generate the tool file and write it to .ai/tools/{input:category}/{input:tool_name}.{ext}

    For Python tools, generate these metadata headers at the top:
    - __version__ = "1.0.0"
    - __tool_type__ = "{input:tool_type}"
    - __executor_id__ = "rye/core/runtimes/python_script_runtime"
    - __category__ = "{input:category}"
    - __tool_description__ = "{input:description}"
    - CONFIG_SCHEMA = dict describing accepted parameters
    - async def execute(params, project_path) function with implementation

    For YAML tools, generate:
    - tool_id, tool_type, version, executor_id, category, description top-level keys
    - parameters list with name, type, required, description per entry
    - config section with tool-specific settings

    Include {input:parameters} definitions in the generated code if provided.

    `rye_execute(item_id="rye/file-system/write", parameters={"path": ".ai/tools/{input:category}/{input:tool_name}.{ext}", "content": "<generated tool code>", "create_dirs": true})`
  </step>

  <step name="sign_tool">
    Validate metadata and sign the newly created tool file.
    `rye_sign(item_type="tool", item_id="{input:category}/{input:tool_name}")`
  </step>
</process>

<success_criteria>
  <criterion>No duplicate tool with the same name exists</criterion>
  <criterion>Tool file created at .ai/tools/{input:category}/{input:tool_name}.{ext} with correct extension</criterion>
  <criterion>Python tools have __version__, __tool_type__, __executor_id__, __category__, CONFIG_SCHEMA, and execute function</criterion>
  <criterion>YAML tools have tool_id, tool_type, version, executor_id, category, description, and parameters</criterion>
  <criterion>Parameter definitions from {input:parameters} included if provided</criterion>
  <criterion>Signature validation passed</criterion>
</success_criteria>
