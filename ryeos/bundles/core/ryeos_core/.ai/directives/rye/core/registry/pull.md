<!-- rye:signed:2026-02-25T07:50:41Z:8beb7eca280dfe5957389abcba55270282d4e10971eb50650be199cf153cfc8a:LomfoeM8nPaPkaCJKEhu3sUn1sUooDheZ9eFlM1gOO89F2TV2RdJ1FJSbvf95iady9cYMpVSokYaJXvZi8OUBw==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:6af2bb2ffaf21c69141c82bf87a48c4dc92c4a852c5971e151906e46d3ae6a8a:kaqPT29CBK4WsIjhTULsce_UmZqVmDAZXJBHdw-ZEy6SvUWxzK0NlJLGWQK3AMIE2Ca8Qo-5wthvs5wfOEWzDg==:9fbfabe975fa5a7f -->
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
