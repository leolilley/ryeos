<!-- rye:signed:2026-02-26T06:42:50Z:5ab93cb93e35317c2fd5cd9e70d29f69df15c0929bdc0d007b3ce561a9b13e3e:mdJQeNA17_xv_lrtNldISROS_5J2dL0NfqnEFzl-E_9VipCYyOKSj6MhcKMWIipCYE0TWpb1ykqGoRhye0lDCg==:4b987fd4e40303ac -->
# Registry Push

Upload an item to the registry.

```xml
<directive name="push" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=push to upload an item to the registry.</description>
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
      Identifier of the item to push
    </input>
    <input name="item_type" type="string" required="true">
      Type of the item (directive, tool, or knowledge)
    </input>
  </inputs>

  <outputs>
    <output name="status">Upload result status</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:item_id} and {input:item_type} are non-empty.
  </step>

  <step name="call_registry_push">
    Call the registry tool with action=push.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "push", "item_id": "{input:item_id}", "item_type": "{input:item_type}"})`
  </step>

  <step name="return_result">
    Return the upload status to the user.
  </step>
</process>
