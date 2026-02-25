<!-- rye:signed:2026-02-25T07:50:41Z:f6f98e6fcba3b01e16593e461e908cb821e04cdb3bb07e396593be4afbb03ac8:q-nfOtZI2m_if8GWRSTJsJQWhoj4jTHiau4qzQirHK7KFJQngZrXiGZGYSZXYxRNChCBF_wojPi8-3B8DT6rCg==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:f5aae73787202ea4f961645030dac3ab9409e132b951cc252f3a72470aa20dc4:EsSb2eJYsLg8nTrP9kTFwjRolkxP_ncZWR4vB1MdUUkJRTKUPXXQ3F8nJx8rooTElZ7Sxii-_LOR_e9wB4OjAw==:9fbfabe975fa5a7f -->
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
    `rye_execute(item_type="tool", item_id="rye/mcp/manager", parameters={"action": "remove", "name": "{input:name}"})`
  </step>

  <step name="return_confirmation">
    Return removal confirmation as {output:removed}.
  </step>
</process>
