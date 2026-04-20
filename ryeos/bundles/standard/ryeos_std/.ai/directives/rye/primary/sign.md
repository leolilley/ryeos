<!-- rye:signed:2026-04-19T09:49:53Z:e83319efc2ec33d74120d4c6ee153376f2df5b458d5f87ce9c233d4ec1afb86d:h+bH+p4DY+u32wIWwDIfF556TW1WcjKblNqPx1B+ZL/fP26bshVp704AdMcO0hvJabYFcIU0wnKYXFZzulQVBw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
