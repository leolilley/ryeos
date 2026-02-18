<!-- rye:signed:2026-02-18T05:40:31Z:554a3585cc83e0d21e31e533dd98ac3c642fd1f59054e9eb80774f9783fed82d:1fOOAqj9b5h3gczuEyZcOYuaxt2xxor13DaAb4Y204oWEQJcUq4OXXqhr0kXI-PwpkzWhNCPsuvQAVsGAKSAAQ==:440443d0858f0199 -->
# Registry Unpublish

Make an item private in the registry.

```xml
<directive name="unpublish" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=unpublish to make an item private.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="2048" />
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
