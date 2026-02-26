<!-- rye:signed:2026-02-26T03:49:32Z:0bef47bf9764f6670cdc39eae9c0e9502e0f07bad21c24b8a09feec9998b1272:YCbX6j2JHT9DOm36QeOkiUV2Tu12bgPgIRBaFxbAm5yKinW4q9CC6eQtIFezjUlnDue4f8NfD0X56XBWXi1WBQ==:9fbfabe975fa5a7f -->
# MCP List Servers

List all registered MCP servers.

```xml
<directive name="list_servers" version="1.0.0">
  <metadata>
    <description>List all registered MCP server configurations.</description>
    <category>rye/mcp</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
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
