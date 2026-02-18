<!-- rye:signed:2026-02-18T05:40:31Z:4369e26b77cf6c0e70bd19a72c5e9c1326dc4953933bfbb892966f4337c31372:sECiCc9z75ZU3VxGEnuPOv9b7T_OiGq2JjwzpmIftkqUGZXEQq_M7YAomSqtzZKpsE1XBgXSFq06sEX7vxnVBQ==:440443d0858f0199 -->
# MCP Discover

Discover available tools on an MCP server.

```xml
<directive name="discover" version="1.0.0">
  <metadata>
    <description>Discover available tools on an MCP server via stdio, HTTP, or SSE transport.</description>
    <category>rye/mcp</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="4096" />
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
    `rye_execute(item_type="tool", item_id="rye/mcp/discover", parameters={"transport": "{input:transport}", "url": "{input:url}", "headers": "{input:headers}", "command": "{input:command}", "args": "{input:args}", "env": "{input:env}"})`
  </step>

  <step name="return_tools">
    Return the discovered tools as {output:tools}.
  </step>
</process>
