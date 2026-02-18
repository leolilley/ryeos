<!-- rye:signed:2026-02-18T05:40:31Z:15218cddba41db1b50c10df0bb92bd1e554622e549c104452fad11ab38233da1:0P1D0VSvoWRXGy-T9emaR6Jgntb88R68uicmue_S62_gHQg57I9hJa77rVwd6w6FBByNIKgfNmJaoW_fb-CZBg==:440443d0858f0199 -->
# Registry Publish

Make an item public in the registry.

```xml
<directive name="publish" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=publish to make an item public.</description>
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
