<!-- rye:signed:2026-02-18T05:40:31Z:3ef372ba9377a1c6db3838681db19639c1b840455204eb9450efee6c61f03a74:RJ1YZ0-MJM6i3JAlZdZapbhV4XoEBnl--yEFwxymaOh6ebZ1v6CczOuJmCkrqAVDxFuxxXB54RnoQGsAD9-FAA==:440443d0858f0199 -->
# Registry Pull

Download an item from the registry.

```xml
<directive name="pull" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=pull to download an item from the registry.</description>
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
