<!-- rye:validated:2026-02-10T02:00:00Z:placeholder -->
# Create Tool

Create a new tool (Python, YAML, etc.) with proper metadata, validate, and sign it.

```xml
<directive name="create_tool" version="2.0.0">
  <metadata>
    <description>Create a tool file with correct metadata headers and parameter schema, then sign it.</description>
    <category>rye/core</category>
    <author>rye-os</author>
    <model tier="haiku" id="claude-3-5-haiku-20241022">Tool creation</model>
    <limits max_turns="8" max_tokens="4096" />
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

  <process>
    <step name="check_existing">
      <description>Search for existing tools with a similar name to avoid duplicates.</description>
      <search item_type="tool" query="{input:tool_name}" />
    </step>

    <step name="write_tool_file">
      <description>
        Generate the tool file and write it to .ai/tools/{input:category}/{input:tool_name}.{ext}.

        The extension is determined by {input:tool_type}:
        - python → .py
        - yaml → .yaml
        - bash → .sh
        - javascript → .js

        For Python tools the LLM MUST generate these metadata headers at the top of the file:
        - __version__ = "1.0.0"
        - __tool_type__ = "{input:tool_type}"
        - __executor_id__ = appropriate executor (e.g., "python_runtime")
        - __category__ = "{input:category}"
        - CONFIG_SCHEMA = dict describing accepted parameters
        Then a main(**kwargs) function with docstring, parameter handling, and a return dict.

        For YAML tools the LLM MUST generate this structure:
        - tool_id, tool_type, version, executor_id, category, description top-level keys
        - parameters list with name, type, required, description per entry
        - config section with tool-specific settings

        Include {input:parameters} definitions in the generated code if provided.
      </description>
      <execute item_type="tool" item_id="rye/file-system/fs_write">
        <param name="path" value=".ai/tools/{input:category}/{input:tool_name}" />
        <param name="content" value="LLM-generated tool code" />
      </execute>
    </step>

    <step name="sign_tool">
      <description>Validate metadata and sign the newly created tool file.</description>
      <sign item_type="tool" item_id="{input:tool_name}" />
    </step>
  </process>

  <outputs>
    <output name="result" type="string">
      Summary: tool name, file path, version, and type.
    </output>
  </outputs>
</directive>
```
