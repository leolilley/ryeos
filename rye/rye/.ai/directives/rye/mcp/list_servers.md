<!-- rye:signed:2026-02-10T02:00:00Z:placeholder:unsigned:unsigned -->
# MCP List Servers

List all registered MCP servers.

```xml
<directive name="list_servers" version="1.0.0">
  <metadata>
    <description>List all registered MCP server configurations.</description>
    <category>rye/mcp</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.mcp.manager</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="include_tools" type="boolean" required="false">Include discovered tools for each server (default false)</input>
  </inputs>

  <outputs>
    <output name="servers">List of registered MCP servers</output>
  </outputs>
</directive>
```

<process>
  <step name="list_servers">
    Call the MCP manager tool with action=list.
    `rye_execute(item_type="tool", item_id="rye/mcp/manager", parameters={"action": "list", "include_tools": "{input:include_tools}"})`
  </step>

  <step name="return_servers">
    Return the server list as {output:servers}.
  </step>
</process>
