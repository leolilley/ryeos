<!-- rye:signed:2026-02-18T05:40:31Z:428dbc6d2098ecff554101e400f32d22aaa174462f83eb66816b233fa21e0e4a:Iers6LBIRpcuXQaFbd58mpcD_5-YARGis4VSbOFKq3IpKvkQr7cGBS9MV4W1NM274A1thRaWf8FavsDgTLHVCQ==:440443d0858f0199 -->
# MCP Connect

Execute a tool call on an MCP server via HTTP or stdio transport.

```xml
<directive name="connect" version="1.0.0">
  <metadata>
    <description>Execute a tool call on an MCP server using HTTP or stdio transport.</description>
    <category>rye/mcp</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="4" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.mcp.connect</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="server_config" type="string" required="false">Path to server YAML configuration file</input>
    <input name="transport" type="string" required="false">Transport type: "http" or "stdio"</input>
    <input name="url" type="string" required="false">Server URL (for HTTP transport)</input>
    <input name="headers" type="object" required="false">HTTP headers (for HTTP transport)</input>
    <input name="command" type="string" required="false">Command to run (for stdio transport)</input>
    <input name="args" type="array" required="false">Command arguments (for stdio transport)</input>
    <input name="tool" type="string" required="true">Name of the MCP tool to call</input>
    <input name="params" type="object" required="false">Parameters to pass to the tool</input>
    <input name="timeout" type="integer" required="false">Request timeout in seconds (default 30)</input>
  </inputs>

  <outputs>
    <output name="result">Tool call result from the MCP server</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that either {input:server_config} or {input:transport} is provided. If neither, halt with an error.
    Validate that {input:tool} is non-empty.
  </step>

  <step name="execute_tool_call">
    Call the MCP connect tool with the provided parameters.
    `rye_execute(item_type="tool", item_id="rye/mcp/connect", parameters={"server_config": "{input:server_config}", "transport": "{input:transport}", "url": "{input:url}", "headers": "{input:headers}", "command": "{input:command}", "args": "{input:args}", "tool": "{input:tool}", "params": "{input:params}", "timeout": "{input:timeout}"})`
  </step>

  <step name="return_result">
    Return the tool call response as {output:result}.
  </step>
</process>
