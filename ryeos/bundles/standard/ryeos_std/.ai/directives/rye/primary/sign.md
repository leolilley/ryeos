<!-- rye:signed:2026-02-26T05:02:40Z:052a33a242a16cd30ae9b89d5b0717cf4d392e962ecd7a3b7d7d713d8f564671:Df95fF423Sy9payTsQ6T7tVmDr5KudutFVet3qz7F8nTgfiOZ1uBO6eH7HSzf_3h1_gjjnJmmXdsOcXadcECBQ==:4b987fd4e40303ac -->
# Sign

Validate and sign a directive, tool, or knowledge item.

```xml
<directive name="sign" version="1.0.0">
  <metadata>
    <description>Validate an item's structure and metadata, then sign it with a cryptographic signature.</description>
    <category>rye/primary</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
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
