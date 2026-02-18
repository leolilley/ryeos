<!-- rye:signed:2026-02-18T05:40:31Z:777fc5083e17e0d586fc6aa52fa28cb133064a6761c457606a483e49d08b0ee3:rsOi-gXdfVOMUhSFnL1FvYOOkjRJC1i-Rsy0U-3bj6ETkFHxh2vbpwDu8cV9mBGnhbL9trwG4tqmRWr8tCv9DQ==:440443d0858f0199 -->
# MCP Add Server

Register a new MCP server and auto-discover its tools.

```xml
<directive name="add_server" version="1.0.0">
  <metadata>
    <description>Register a new MCP server configuration and auto-discover its available tools.</description>
    <category>rye/mcp</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="4" max_tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.mcp.manager</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="name" type="string" required="true">Server name identifier</input>
    <input name="transport" type="string" required="true">Transport type: "http" or "stdio"</input>
    <input name="url" type="string" required="false">Server URL (for HTTP transport)</input>
    <input name="headers" type="object" required="false">HTTP headers (for HTTP transport)</input>
    <input name="command" type="string" required="false">Command to run (for stdio transport)</input>
    <input name="args" type="array" required="false">Command arguments (for stdio transport)</input>
    <input name="env" type="object" required="false">Environment variables for the server process</input>
    <input name="scope" type="string" required="false">Registration scope: "project" or "user" (default "project")</input>
    <input name="timeout" type="integer" required="false">Connection timeout in seconds (default 30)</input>
  </inputs>

  <outputs>
    <output name="server">Registered server configuration with discovered tools</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:name} is non-empty and {input:transport} is "http" or "stdio". If not, halt with an error.
  </step>

  <step name="add_server">
    Call the MCP manager tool with action=add.
    `rye_execute(item_type="tool", item_id="rye/mcp/manager", parameters={"action": "add", "name": "{input:name}", "transport": "{input:transport}", "url": "{input:url}", "headers": "{input:headers}", "command": "{input:command}", "args": "{input:args}", "env": "{input:env}", "scope": "{input:scope}", "timeout": "{input:timeout}"})`
  </step>

  <step name="return_server">
    Return the registered server details as {output:server}.
  </step>
</process>
