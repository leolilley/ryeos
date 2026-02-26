<!-- rye:signed:2026-02-26T06:42:50Z:ea4c0c674f05939d7aaf6e3ddf61e96c6c896fbe8344407d9e8fdec3c98b9782:a8Eh2FiaqfC2opQ67PrWqKHoEmLcMH8txuWhhADxbNa9THOkJLuCMwtVVACDJcjp_G2I4ARhGR_dQTkBMdEIBw==:4b987fd4e40303ac -->
# Create Tool

Create a new tool (Python, YAML, etc.) with proper metadata, validate, and sign it.

```xml
<directive name="create_tool" version="3.0.0">
  <metadata>
    <description>Create a tool file with correct metadata headers and parameter schema, then sign it.</description>
    <category>rye/core</category>
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
      <sign>
        <tool>*</tool>
      </sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="tool_name" type="string" required="true">
      Unique identifier in kebab-case (e.g., "deploy-service", "validate-jwt")
    </input>
    <input name="category" type="string" required="true">
      Directory path relative to .ai/tools/ (e.g., "utilities", "deployment/kubernetes")
    </input>
    <input name="tool_type" type="string" required="true">
      Type of tool: python, yaml, bash, javascript, etc.
    </input>
    <input name="description" type="string" required="true">
      What does this tool do?
    </input>
    <input name="parameters" type="string" required="false">
      Comma-separated parameter definitions (name:type:required, e.g., "service_name:string:true, replicas:integer:false")
    </input>
  </inputs>

  <outputs>
    <output name="tool_path">Path to the created tool file</output>
    <output name="signed">Whether the tool was successfully signed</output>
  </outputs>
</directive>
```

<process>
  <step name="check_existing">
    Search for existing tools with a similar name to avoid duplicates.
    `rye_search(item_type="tool", query="{input:tool_name}")`
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

    `rye_execute(item_type="tool", item_id="rye/file-system/write", parameters={"path": ".ai/tools/{input:category}/{input:tool_name}.{ext}", "content": "<generated tool code>", "create_dirs": true})`
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

