<!-- rye:signed:2026-02-26T06:42:50Z:85a5a45b18a44277801c3665306130004fda37200c4e0689d5d2d05fdab95ec1:b_VonxrLDI1ijcIBWdUqUDnmbuIzQ3UH9cSE63hNOYPG8NIijiF3k4QzwttfnwXS-i7zt-JryX1kcqZpVhPbCA==:4b987fd4e40303ac -->
# Registry Delete

Remove an item from the registry.

```xml
<directive name="delete" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=delete to remove an item from the registry.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
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
