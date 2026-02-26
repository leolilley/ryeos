<!-- rye:signed:2026-02-25T07:50:41Z:1d7b49bef84cc21979cb5098241cd439995f50675368ad8a993e9717ac3587de:psk1DyMLf_cdN4CWuM9NZq4QkxS0ckjBqiB7wzizfx902iAw7qb7tg7gkojckNHq8j-23XlzDaVwZ2jvF08rBg==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:d5d67d88e60529021f8666aefb844e6daf2de0687b856e34b212ddef95f7605f:fDT5Qwnl9DhB_1noIfWStLs_6LhEhZiPMRsLi8mQNDE56gOd4nWhYUn0PUfCcKVd3aJ0cm7sfI0TRysl60P5CQ==:9fbfabe975fa5a7f -->
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
