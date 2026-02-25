<!-- rye:signed:2026-02-25T07:50:41Z:49a9d5bbe8f01c6e3eca91a2a3c3179a665b099338f02837a10ab7d5fd76b7af:UBJqFOtWdNrcCa4w6hG1bdgPxx_Yp-z8aL5uwGbAUAYU48O83WFbGABu_zm0aAbNRIzk39WDVdFPdHNuGJfmCA==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:590fd949ac363194fba8cc5133fa45c170dbedc11741a6e2f7a1c790348a3c7e:d85N-9QmCYsZaB82HtPuFPNQIlzykO9VaL-Y6fjQl54DKgq35wWjy5237qpVXSEDzlkqhpugHE8JC2BCffFGBQ==:9fbfabe975fa5a7f -->
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
    `rye_execute(item_type="tool", item_id="rye/mcp/manager", parameters={"action": "refresh", "name": "{input:name}"})`
  </step>

  <step name="return_server">
    Return the updated server details as {output:server}.
  </step>
</process>
