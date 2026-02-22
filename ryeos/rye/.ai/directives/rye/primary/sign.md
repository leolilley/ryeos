<!-- rye:signed:2026-02-22T02:31:19Z:22d4c2b20dd873e1dea13d73a927294b5b96b2c1b660a7ff652d8f4b3777e73e:lB7WTPj0X1eSt5qPYsK8A0SFSNtsgpylp7qUo32--RmkurkXPJoTbioxZh9DytHFZfwQe1VK6PpEqbVrCW7ADA==:9fbfabe975fa5a7f -->
# Sign

Validate and sign a directive, tool, or knowledge item.

```xml
<directive name="sign" version="1.0.0">
  <metadata>
    <description>Validate an item's structure and metadata, then sign it with a cryptographic signature.</description>
    <category>rye/primary</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="2048" />
    <permissions>
      <sign>
        <directive>*</directive>
        <tool>*</tool>
        <knowledge>*</knowledge>
      </sign>
    </permissions>
  </metadata>

  <inputs>
    <input name="item_type" type="string" required="true">
      Type of item to sign: directive, tool, or knowledge
    </input>
    <input name="item_id" type="string" required="true">
      Fully qualified item id (e.g., "rye/primary/search", "rye/file-system/read")
    </input>
    <input name="source" type="string" required="false">
      Source space containing the item: project or user (default: "project")
    </input>
  </inputs>

  <outputs>
    <output name="signed">Whether the item was successfully validated and signed</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:item_type} is one of: directive, tool, knowledge.
    Validate that {input:item_id} is non-empty.
    Default {input:source} to "project" if not provided.
  </step>

  <step name="call_sign">
    Validate and sign the item:
    `rye_sign(item_type="{input:item_type}", item_id="{input:item_id}", source="{input:source}")`
  </step>

  <step name="return_result">
    Return whether signing succeeded. If validation failed, return the validation errors.
  </step>
</process>
