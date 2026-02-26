<!-- rye:signed:2026-02-26T05:02:40Z:1d7b49bef84cc21979cb5098241cd439995f50675368ad8a993e9717ac3587de:ziJK9Fh9bjuswTctcYv4hYTmCYKZVo3RTd9TAEZ3CXyLiF0-O9cGhweO2a8z2sCPAHALuYnA7Jeql6CKc10rBA==:4b987fd4e40303ac -->
# Load

Load or copy a directive, tool, or knowledge item by id and source.

```xml
<directive name="load" version="1.0.0">
  <metadata>
    <description>Load item content for inspection or copy between project and user spaces.</description>
    <category>rye/primary</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="4096" />
    <permissions>
      <load>
        <directive>*</directive>
        <tool>*</tool>
        <knowledge>*</knowledge>
      </load>
    </permissions>
  </metadata>

  <inputs>
    <input name="item_type" type="string" required="true">
      Type of item to load: directive, tool, or knowledge
    </input>
    <input name="item_id" type="string" required="true">
      Fully qualified item id (e.g., "rye/core/create_directive", "rye/file-system/read")
    </input>
    <input name="source" type="string" required="false">
      Source space to load from: project, user, or system (default: "project")
    </input>
    <input name="destination" type="string" required="false">
      Destination space to copy to: project or user. If omitted, item is loaded for inspection only.
    </input>
  </inputs>

  <outputs>
    <output name="content">The loaded item content and metadata</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:item_type} is one of: directive, tool, knowledge.
    Validate that {input:item_id} is non-empty.
    Default {input:source} to "project" if not provided.
  </step>

  <step name="call_load">
    Execute the load tool:
    `rye_load(item_type="{input:item_type}", item_id="{input:item_id}", source="{input:source}", destination="{input:destination}")`
  </step>

  <step name="return_result">
    Return the loaded item content and metadata to the caller.
  </step>
</process>
