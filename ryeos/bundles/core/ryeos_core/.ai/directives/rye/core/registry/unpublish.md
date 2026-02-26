<!-- rye:signed:2026-02-26T03:49:26Z:a36fddc037bd6c63cc28c5ac06c97e13d7fc309a87c6c82df2dbde0e2fb7c70f:k9s21mqXv-cQx7rtTcoA_jDs8TPp-keraoHpkBKrRXcLGTrEF35SGgxtz4fePBcmttHcxEZAIezc_bXlOjx6AQ==:9fbfabe975fa5a7f -->
# Registry Unpublish

Make an item private in the registry.

```xml
<directive name="unpublish" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=unpublish to make an item private.</description>
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
      Identifier of the item to unpublish
    </input>
    <input name="item_type" type="string" required="true">
      Type of the item (directive, tool, or knowledge)
    </input>
  </inputs>

  <outputs>
    <output name="status">Unpublish result status</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:item_id} and {input:item_type} are non-empty.
  </step>

  <step name="call_registry_unpublish">
    Call the registry tool with action=unpublish.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "unpublish", "item_id": "{input:item_id}", "item_type": "{input:item_type}"})`
  </step>

  <step name="return_result">
    Return the unpublish status to the user.
  </step>
</process>
