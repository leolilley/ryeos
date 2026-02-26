<!-- rye:signed:2026-02-26T03:49:26Z:7c0753bfe3f3caaa7fcb6dc29cce5625fb2ef616a24ae404e8cdace63852d4ca:SLuVrnHH_JzXa7uJAxMVJYeFdqLZfUmYta1LpNVodJq0jfF7XUf1sgv6Hs0cgbCsLgptOlaYuiYakzhOmKVaBA==:9fbfabe975fa5a7f -->
# Registry Publish

Make an item public in the registry.

```xml
<directive name="publish" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=publish to make an item public.</description>
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
      Identifier of the item to publish
    </input>
    <input name="item_type" type="string" required="true">
      Type of the item (directive, tool, or knowledge)
    </input>
  </inputs>

  <outputs>
    <output name="status">Publish result status</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:item_id} and {input:item_type} are non-empty.
  </step>

  <step name="call_registry_publish">
    Call the registry tool with action=publish.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "publish", "item_id": "{input:item_id}", "item_type": "{input:item_type}"})`
  </step>

  <step name="return_result">
    Return the publish status to the user.
  </step>
</process>
