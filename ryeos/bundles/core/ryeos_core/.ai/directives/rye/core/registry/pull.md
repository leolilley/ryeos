<!-- rye:signed:2026-02-26T06:42:50Z:8beb7eca280dfe5957389abcba55270282d4e10971eb50650be199cf153cfc8a:DJoR7qSxZBI1QEh41evSthzp5wvgolanGOZ-wU7hCRdm2aPlcNU2Y7nl9M1CINiZROvHj8s4uoEgbRJLPUJwDA==:4b987fd4e40303ac -->
# Registry Pull

Download an item from the registry.

```xml
<directive name="pull" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=pull to download an item from the registry.</description>
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
      Identifier of the item to pull
    </input>
    <input name="item_type" type="string" required="true">
      Type of the item (directive, tool, or knowledge)
    </input>
  </inputs>

  <outputs>
    <output name="item">The downloaded item</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:item_id} and {input:item_type} are non-empty.
  </step>

  <step name="call_registry_pull">
    Call the registry tool with action=pull.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "pull", "item_id": "{input:item_id}", "item_type": "{input:item_type}"})`
  </step>

  <step name="return_result">
    Return the downloaded item to the user.
  </step>
</process>
