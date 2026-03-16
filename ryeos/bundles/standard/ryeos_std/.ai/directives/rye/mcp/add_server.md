<!-- rye:signed:2026-03-16T09:53:45Z:9bedc1e2c8923e788a451c6fe5030bef5bd507211cd00bff9449e8150f5a6794:thO-2xez_rYyS0T_68mkhN41YFSTPosZLmfZ2jg1riD-XXyCPzH4_3BbL0yDbVhJiNUOn6PZ9ULgCo0kurrCBw==:4b987fd4e40303ac -->
<!-- rye:unsigned -->
# MCP Add Server

Register a new MCP server with Rye OS end-to-end — creates server config, discovers tools, generates per-tool YAML wrappers, signs everything, and verifies the setup.

```xml
<directive name="add_server" version="2.0.0">
  <metadata>
    <description>Register an MCP server end-to-end: create server config YAML, discover tools, generate per-tool wrappers, sign all files, verify setup, and output directive permission strings.</description>
    <category>rye/mcp</category>
    <author>rye-os</author>
    <model tier="general" />
    <limits turns="10" tokens="50000" spend="0.25" />
    <permissions>
      <execute>
        <tool>rye.mcp.*</tool>
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
    <input name="name" type="string" required="true">
      Server name identifier (e.g., "campaign-kiwi", "context7"). Used as directory name and config filename.
    </input>
    <input name="transport" type="string" required="true">
      Transport type: "http" or "stdio"
    </input>
    <input name="url" type="string" required="false">
      Server URL (http only). E.g., "https://mcp.example.com/mcp"
    </input>
    <input name="headers" type="object" required="false">
      HTTP headers as {key: value} (http only)
    </input>
    <input name="command" type="string" required="false">
      Command to launch the server (stdio only). E.g., "/path/to/venv/bin/python"
    </input>
    <input name="args" type="array" required="false">
      Command arguments as list (stdio only). E.g., ["-m", "src.server"]
    </input>
    <input name="cwd" type="string" required="false">
      Working directory for the server process (stdio only). E.g., "/path/to/project"
    </input>
    <input name="mcp_server_env" type="object" required="false">
      Environment variables as {key: value} for the MCP server process.
      Named mcp_server_env (not env) to avoid collision with the Rye executor's env handling.
      Supports ${VAR} and ${VAR:-default} expansion from .env files.
    </input>
    <input name="scope" type="string" required="false" default="project">
      Where to store configs: "project" (.ai/) or "user" (~/.ai/)
    </input>
    <input name="timeout" type="integer" required="false" default="30">
      Connection timeout in seconds
    </input>
  </inputs>

  <outputs>
    <output name="server_config_path">Path to the created server config YAML</output>
    <output name="tool_count">Number of tools discovered from the server</output>
    <output name="tool_ids">List of tool item_ids (e.g., mcp/campaign-kiwi/execute)</output>
    <output name="permission_snippet">XML permission block for use in directives</output>
    <output name="status">success or error with details</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate required inputs based on transport type:
    - For "stdio": {input:command} is required
    - For "http": {input:url} is required
    - {input:name} must be non-empty
    - {input:transport} must be "http" or "stdio"

    If validation fails, return an error with the correct usage.
  </step>

  <step name="check_existing">
    Check if a server named {input:name} already exists:
    `rye_execute(item_type="tool", item_id="rye/mcp/manager", parameters={"action": "list", "include_tools": true})`

    If the server exists:
    - Ask if user wants to refresh (re-discover tools) or abort
    - To refresh: `rye_execute(item_type="tool", item_id="rye/mcp/manager", parameters={"action": "refresh", "name": "{input:name}"})`
    - Then skip to the sign step
  </step>

  <step name="register_server">
    Call the MCP manager to add the server. This creates the server config YAML
    at .ai/tools/mcp/servers/{input:name}.yaml, connects to the server, discovers
    all available tools, and generates per-tool YAML wrappers at
    .ai/tools/mcp/{input:name}/{tool_name}.yaml.

    `rye_execute(item_type="tool", item_id="rye/mcp/manager", parameters={"action": "add", "name": "{input:name}", "transport": "{input:transport}", "scope": "{input:scope}", "url": "{input:url}", "headers": {input:headers}, "command": "{input:command}", "args": {input:args}, "mcp_server_env": {input:mcp_server_env}, "cwd": "{input:cwd}", "timeout": {input:timeout}})`

    If this fails, check:
    - For stdio: is the command path correct? Is the venv activated?
    - For http: is the URL reachable? Are auth headers needed?
    - Common fix: ensure the server process can start and the MCP SDK is installed
  </step>

  <step name="sign_all">
    Sign the server config and all generated tool YAMLs:
    `rye_sign(item_type="tool", item_id="mcp/servers/{input:name}")`
    `rye_sign(item_type="tool", item_id="mcp/{input:name}/*")`
  </step>

  <step name="verify">
    Verify the registration by listing servers with tools:
    `rye_execute(item_type="tool", item_id="rye/mcp/manager", parameters={"action": "list", "include_tools": true})`

    Confirm {input:name} appears with the correct transport and discovered tools.
  </step>

  <step name="output_summary">
    Build and return the summary:

    - server_config_path: .ai/tools/mcp/servers/{input:name}.yaml
    - tool_count: number from the add/refresh result
    - tool_ids: list of discovered tool item_ids (e.g., mcp/{input:name}/search)
    - permission_snippet: ready-to-paste XML for directives:

    ```xml
    <permissions>
      <execute>
        <tool>mcp.{input:name}.*</tool>
      </execute>
    </permissions>
    ```

    - status: success or error details

    Print the full summary so the user can copy permission strings into their directives.
  </step>
</process>

<success_criteria>
<criterion>Server config YAML created at the correct path for the chosen scope</criterion>
<criterion>Tools successfully discovered from the MCP server</criterion>
<criterion>Per-tool YAML wrappers generated for each discovered tool</criterion>
<criterion>All created files signed</criterion>
<criterion>Server and tools visible in manager list output</criterion>
<criterion>Permission snippet printed for directive use</criterion>
</success_criteria>
