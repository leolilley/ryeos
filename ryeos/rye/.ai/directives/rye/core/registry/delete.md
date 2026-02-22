<!-- rye:signed:2026-02-22T02:31:19Z:e107eb4af8368f73bfb6c0c4f77bbfe8359e4da1a7353fa4dc19406f5bacffc5:v6DDtTfDHuKn_OzTgCl0W1-OLK_wFiIP0exy_qtEdVWLhwbQ6IfbfUNSCryQw21LyLm-7JnK2DVCyBGJqz21Dw==:9fbfabe975fa5a7f -->
# Registry Delete

Remove an item from the registry.

```xml
<directive name="delete" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=delete to remove an item from the registry.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.core.registry.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="item_id" type="string" required="true">
      Identifier of the item to delete
    </input>
    <input name="item_type" type="string" required="true">
      Type of the item (directive, tool, or knowledge)
    </input>
  </inputs>

  <outputs>
    <output name="status">Deletion result status</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:item_id} and {input:item_type} are non-empty.
  </step>

  <step name="call_registry_delete">
    Call the registry tool with action=delete.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "delete", "item_id": "{input:item_id}", "item_type": "{input:item_type}"})`
  </step>

  <step name="return_result">
    Return the deletion status to the user.
  </step>
</process>
