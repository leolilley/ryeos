<!-- rye:signed:2026-02-20T01:09:07Z:9595bc9c02a9104e2bcdd7f04d8f0a9906b730722c9e7fd6497437e852f0a8f6:w5_0cqGJuZPKIumsRDJvhRhqQovJ0ceVm7UxLL7dVupLY7Ub6sHpZae_URYHinQ5r4j8lTLpMweH09d0xK-RAA==:440443d0858f0199 -->
# MCP List Servers

List all registered MCP servers.

```xml
<directive name="list_servers" version="1.0.0">
  <metadata>
    <description>List all registered MCP server configurations.</description>
    <category>rye/mcp</category>
    <author>rye-os</author>
    <model tier="fast" />
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
