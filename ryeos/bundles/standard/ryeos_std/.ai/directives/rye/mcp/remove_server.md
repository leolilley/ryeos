<!-- rye:signed:2026-04-19T09:49:53Z:f238434fc926ab65bafc1685b1c6183fac182b1ba8d334c66c9208965e71e22b:5vvSQXJGLAPAmsJNVja9ee4EJwlMYV2RgN85egY0iVDCfQa762NnGV9BV+bXWUwuueGtOhgE92okDTVBF1XyBA==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
# MCP Remove Server

Remove a registered MCP server.

```xml
<directive name="remove_server" version="1.0.0">
  <metadata>
    <description>Remove a registered MCP server configuration and its discovered tools.</description>
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
    <input name="name" type="string" required="true">Name of the server to remove</input>
  </inputs>

  <outputs>
    <output name="removed">Confirmation that the server was removed</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:name} is non-empty. If empty, halt with an error.
  </step>

  <step name="remove_server">
    Call the MCP manager tool with action=remove.
    `rye_execute(item_id="rye/mcp/manager", parameters={"action": "remove", "name": "{input:name}"})`
  </step>

  <step name="return_confirmation">
    Return removal confirmation as {output:removed}.
  </step>
</process>
