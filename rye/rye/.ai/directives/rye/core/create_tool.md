# Create Tool

Create new tools in Python, JavaScript, YAML, or other formats, validate, and sign.

```xml
<directive name="create_tool" version="1.0.0">
  <metadata>
    <description>Create extensible tools with proper metadata, parameter schema, and validation.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="balanced" fallback="general">
      Standard workflow for tool creation
    </model>
    <context_budget>
      <estimated_usage>15%</estimated_usage>
      <step_count>5</step_count>
      <spawn_threshold>50%</spawn_threshold>
    </context_budget>
    <permissions>
      <execute>
        <tool>rye.file-system.*</tool>
      </execute>
      <search>
        <tool>*</tool>
      </search>
      <load>
        <tool>*</tool>
      </load>
      <sign>
        <tool>*</tool>
      </sign>
    </permissions>
    <relationships>
      <suggests>edit_tool</suggests>
    </relationships>
  </metadata>

  <context>
    <tech_stack>any</tech_stack>
  </context>

  <inputs>
    <input name="tool_id" type="string" required="true">
      Unique identifier in kebab-case (e.g., "deploy-service", "read-file", "validate-jwt")
    </input>
    <input name="category" type="string" required="true">
      Directory path relative to .ai/tools/ (e.g., "utilities", "deployment/kubernetes", "security")
    </input>
    <input name="tool_type" type="string" required="true">
      Type of tool: python, javascript, yaml, bash, http, runtime, library, etc.
    </input>
    <input name="executor_id" type="string" required="true">
      Executor that runs this tool (e.g., python_runtime, node_runtime, http_client, or null for primitives)
    </input>
    <input name="description" type="string" required="true">
      What does this tool do? (e.g., "Deploy service to Kubernetes cluster")
    </input>
    <input name="parameters" type="string" required="false">
      Comma-separated parameter definitions (name:type:required, e.g., "service_name:string:true, replicas:integer:false")
    </input>
    <input name="requires_capabilities" type="string" required="false">
      Comma-separated capabilities (e.g., "fs.read, shell.execute, net.call")
    </input>
    <input name="tags" type="string" required="false">
      Comma-separated tags (3-5 recommended)
    </input>
  </inputs>

  <process>
    <step name="validate_inputs">
      <description>Validate all required inputs</description>
      <action><![CDATA[
1. tool_id must be kebab-case alphanumeric
2. category must be non-empty string
3. tool_type must be non-empty string (python, javascript, yaml, bash, http, runtime, library, etc.)
4. executor_id must be non-empty or "null" for primitives
5. description must be non-empty
6. If executor_id = "null": tool_type must be "primitive" or "library"

Halt if any validation fails.
      ]]></action>
    </step>

    <step name="determine_file_path">
      <description>Calculate file path and extension from tool_type</description>
      <action><![CDATA[
Extensions by tool_type:
- python → .py
- javascript → .js
- yaml → .yaml
- bash → .sh
- http → .yaml (HTTP tools are YAML-based)
- runtime → .py or .js depending on language
- library → .py or .js
- other → infer from tool_type (e.g., go → .go)

File location: .ai/tools/{category}/{tool_id}.{ext}

Examples:
- category="utilities", tool_type="python" → .ai/tools/utilities/deploy-service.py
- category="deployment/kubernetes", tool_type="yaml" → .ai/tools/deployment/kubernetes/deploy-k8s.yaml
- category="security", tool_type="javascript" → .ai/tools/security/validate-jwt.js

Create parent directories as needed.
      ]]></action>
    </step>

    <step name="create_tool_code">
      <description>Generate tool file with metadata headers</description>
      <action>Tool code structure by format:

For Python tools:
- Metadata: __version__, __tool_type__, __executor_id__, __category__
- Function: main(**kwargs) with docstring and parameter documentation
- Return: dict with status and results

For JavaScript tools:
- Metadata: JSDoc comments with @version, @tool_type, @executor_id, @category
- Function: main(args) async function
- Return: status object

For YAML tools:
- Metadata: tool_id, tool_type, version, executor_id, category, description
- Parameters section with name, type, required, description
- Config section with tool-specific settings
- Requires section with capability declarations

For Bash tools:
- Shebang: #!/bin/bash
- Metadata comments: tool_type, executor_id, category, version
- Function: main() with argument handling
- Echo results and exit status
      </action>
    </step>

    <step name="add_parameters_and_schema">
      <description>Define parameters and input schema</description>
      <action><![CDATA[
For Python tools, add to docstring:

Args:
  param_name (str): Description
  count (int): Number of items (1-10)

For YAML tools, add parameters section:

parameters:
  - name: service_name
    type: string
    required: true
    description: "Service name"
    pattern: "^[a-z][a-z0-9-]*$"
  
  - name: replicas
    type: integer
    required: false
    default: 3
    minimum: 1
    maximum: 10

For all tools, document:
- name: Parameter identifier
- type: string, integer, float, boolean, object, array
- required: true/false
- default: Default value
- description: Human-readable description
- constraints: minimum, maximum, pattern, enum, minLength, maxLength
      ]]></action>
    </step>

    <step name="validate_and_sign">
      <description>Validate tool metadata and generate signature</description>
      <action><![CDATA[
Run mcp_rye sign to validate and sign the tool:

mcp_rye_execute(
  item_type="tool",
  action="sign",
  item_id="{tool_id}",
  parameters={"location": "project"},
  project_path="{project_path}"
)

This:
1. Validates metadata headers (version, tool_type, executor_id, category)
2. Checks category matches file path
3. Verifies executor_id is valid (or null for primitives)
4. Parses parameters/schema for syntax
5. Generates SHA256 content hash
6. Creates validation signature comment at top
7. Makes tool discoverable in registry

If validation fails: fix errors and re-run sign.
      ]]></action>
      <verification>
        <check>File has signature comment at top</check>
        <check>Metadata headers are valid</check>
        <check>Category matches file path</check>
        <check>executor_id is recognized or null</check>
      </verification>
    </step>
  </process>

  <success_criteria>
    <criterion>Tool file created at correct path with proper extension</criterion>
    <criterion>All required metadata headers present</criterion>
    <criterion>Parameters documented with types and descriptions</criterion>
    <criterion>Signature validation comment added</criterion>
    <criterion>Tool is discoverable via search</criterion>
  </success_criteria>

  <outputs>
    <success><![CDATA[
✓ Created tool: {tool_id}
Location: .ai/tools/{category}/{tool_id}.{extension}
Version: 1.0.0
Type: {tool_type}
Executor: {executor_id}

Next steps:
- Test: Run tool {tool_id} with test parameters
- Edit: Update tool code and re-sign
- Document: Add examples and use cases
- Register: Publish to tool registry (optional)
    ]]></success>
    <failure><![CDATA[
✗ Failed to create tool: {tool_id}
Error: {error}

Common fixes:
- tool_id must be kebab-case (deploy-service, not DeployService)
- category must match target directory path
- Metadata headers must be valid (version, tool_type, executor_id, category)
- executor_id must be recognized (python_runtime, node_runtime, http_client) or null
- File extension must match tool_type (.py for python, .js for javascript, .yaml for yaml)
    ]]></failure>
  </outputs>
</directive>
```
