<!-- rye:signed:2026-04-19T09:49:53Z:c4c8b3ce2854ec922c7ec470c541d95b7f26a4294102be4e701843a00ec18045:JYMebgljHLXvxr5jXXJ3CSqkrCuX7HQwES1F6kNx2fu9ooAOYFico2TPr2uaTai/JU3mQLpwcxu40NYO0nz3BQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
    `rye_execute(item_id="rye/mcp/manager", parameters={"action": "list", "include_tools": "{input:include_tools}"})`
  </step>

  <step name="return_servers">
    Return the server list as {output:servers}.
  </step>
</process>
