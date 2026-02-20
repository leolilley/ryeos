<!-- rye:signed:2026-02-20T01:09:07Z:d5d67d88e60529021f8666aefb844e6daf2de0687b856e34b212ddef95f7605f:AAtRRMXCszj2R468hXA_fNaUuFW3pSHdfzDRQ41sOoPrJkFH0EDGHKy3nhqQ4d5rGgFwxNscKSsPJ_nmMdSYBQ==:440443d0858f0199 -->
# Load

Load or copy a directive, tool, or knowledge item by id and source.

```xml
<directive name="load" version="1.0.0">
  <metadata>
    <description>Load item content for inspection or copy between project and user spaces.</description>
    <category>rye/primary</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="3" max_tokens="4096" />
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
