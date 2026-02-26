<!-- rye:signed:2026-02-26T05:02:40Z:0bef47bf9764f6670cdc39eae9c0e9502e0f07bad21c24b8a09feec9998b1272:RDUTB97o_h0CVZMtk8wxi1uS8GWhkBg7abugR9GIeHAhSEsUPxSXx9q0r15ADK33L5gMbDxFzhdzlGWd1z9ZCw==:4b987fd4e40303ac -->
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
