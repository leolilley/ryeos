<!-- rye:signed:2026-02-26T05:02:40Z:0c1b0d05b5f3fb512ae69268df7e5b5ffc65554765375493f3ca13ba7d2eab6d:C7aP-5DNJVM8fgul0Z31tR0xhxrV3o6ub6nbS7QqBNBWEp4cooqfMv65lEmrzCQ1cNOLlvkJZl8aPJjAFZWaAA==:4b987fd4e40303ac -->
# MCP Connect

Execute a tool call on an MCP server via HTTP or stdio transport.

```xml
<directive name="connect" version="1.0.0">
  <metadata>
    <description>Execute a tool call on an MCP server using HTTP or stdio transport.</description>
    <category>rye/mcp</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" />
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
