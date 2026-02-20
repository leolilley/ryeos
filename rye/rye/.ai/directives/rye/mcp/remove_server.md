<!-- rye:signed:2026-02-20T01:09:07Z:f5aae73787202ea4f961645030dac3ab9409e132b951cc252f3a72470aa20dc4:-A-V1B57GriAV9gF5oG66a6PbIRys-kKrPum_y-Akq0BvNwxTqcKoAQaLzOB4yriWSt6ROQOAWf6txcqJ5kkBg==:440443d0858f0199 -->
# MCP Remove Server

Remove a registered MCP server.

```xml
<directive name="remove_server" version="1.0.0">
  <metadata>
    <description>Remove a registered MCP server configuration and its discovered tools.</description>
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
    `rye_execute(item_type="tool", item_id="rye/mcp/manager", parameters={"action": "remove", "name": "{input:name}"})`
  </step>

  <step name="return_confirmation">
    Return removal confirmation as {output:removed}.
  </step>
</process>
