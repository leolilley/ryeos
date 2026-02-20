<!-- rye:signed:2026-02-20T01:09:07Z:e78d5f0473ca2515c542e7bcc780484d3e3d0c9fd7ed05b9792b8f9dc32b1887:argC2eLmbypk3KL3PD8VEpgGaHWgrP4uEQCugdSD4Aqf2zbeU9vBuln72_br-tvqEA0zYnYqBKuehwH3ZjtdBA==:440443d0858f0199 -->
# Registry Publish

Make an item public in the registry.

```xml
<directive name="publish" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=publish to make an item public.</description>
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
