<!-- rye:signed:2026-04-19T09:49:53Z:c87e975dd213e0a9b3a268f4257849376f467e537a6ee19406a44a89a64b3327:EXQSW2zLjVSTqNpElfl3Db1vR4Y8Xb9VgHgg45bnI/jTfuUx3xRfVErqlRtzPFtuJxCg7lWXA7HxT6acvPqYAQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
# MCP Refresh Server

Refresh a registered MCP server's tool discovery.

```xml
<directive name="refresh_server" version="1.0.0">
  <metadata>
    <description>Re-discover tools on a registered MCP server and update its configuration.</description>
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
    <input name="name" type="string" required="true">Name of the server to refresh</input>
  </inputs>

  <outputs>
    <output name="server">Updated server configuration with refreshed tools</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:name} is non-empty. If empty, halt with an error.
  </step>

  <step name="refresh_server">
    Call the MCP manager tool with action=refresh.
    `rye_execute(item_id="rye/mcp/manager", parameters={"action": "refresh", "name": "{input:name}"})`
  </step>

  <step name="return_server">
    Return the updated server details as {output:server}.
  </step>
</process>
