<!-- rye:signed:2026-02-18T05:40:31Z:f08502e65ec7f8a96e791288d9234e29c491b61c01abb899d67e0bb8572379df:bKVDfRij86sInb0BQ2qvHdCbaezwYghXpzNY9zDf248OdhV40_ZOnuAVmck5d7CEzWtJ-cF48WvuMD-Q0x_pCw==:440443d0858f0199 -->
# Registry Push

Upload an item to the registry.

```xml
<directive name="push" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=push to upload an item to the registry.</description>
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
