<!-- rye:signed:2026-04-19T09:49:53Z:3bb1edbdbee11a67718106226137e396082d60756f3fdc7a79701eebc1df3559:OODwXm8AXu8Wgw79jb8H6UOgvSFhBee1ksMpXCuDYofQ2grh36Ck/h2iasqHqfqTzEgL2jRoXZZehBERhdi9AQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
# MCP Discover

Discover available tools on an MCP server.

```xml
<directive name="discover" version="1.0.0">
  <metadata>
    <description>Discover available tools on an MCP server via stdio, HTTP, or SSE transport.</description>
    <category>rye/mcp</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.mcp.discover</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="transport" type="string" required="true">Transport type: "stdio", "http", or "sse"</input>
    <input name="url" type="string" required="false">Server URL (for HTTP/SSE transport)</input>
    <input name="headers" type="object" required="false">HTTP headers</input>
    <input name="command" type="string" required="false">Command to run (for stdio transport)</input>
    <input name="args" type="array" required="false">Command arguments (for stdio transport)</input>
    <input name="env" type="object" required="false">Environment variables for the server process</input>
  </inputs>

  <outputs>
    <output name="tools">List of available tools with names, descriptions, and input schemas</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:transport} is one of "stdio", "http", or "sse". If not, halt with an error.
  </step>

  <step name="discover_tools">
    Call the MCP discover tool with the provided parameters.
    `rye_execute(item_id="rye/mcp/discover", parameters={"transport": "{input:transport}", "url": "{input:url}", "headers": "{input:headers}", "command": "{input:command}", "args": "{input:args}", "env": "{input:env}"})`
  </step>

  <step name="return_tools">
    Return the discovered tools as {output:tools}.
  </step>
</process>
