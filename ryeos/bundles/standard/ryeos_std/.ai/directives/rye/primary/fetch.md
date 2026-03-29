<!-- rye:signed:2026-03-29T06:39:14Z:73b26ead3d76c88157bd27f9176666561df47ea5d2d6a947197c1d20485e8271:1hal57nuBIyfdr3o3c-VNGcmpkr86Qaa_JL002a2UmTXRc1Cgt1SnPm6vMH6z8khf2WGXPRHaVbwrWkVDqp7Aw==:4b987fd4e40303ac -->
# Fetch

Resolve items by ID or discover by query.

```xml
<directive name="fetch" version="1.0.0">
  <metadata>
    <description>Resolve a name to items. Two modes: ID mode (item_id) returns content, query mode (query+scope) discovers matches.</description>
    <category>rye/primary</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" />
    <permissions>
      <fetch>
        <tool>*</tool>
        <directive>*</directive>
        <knowledge>*</knowledge>
      </fetch>
    </permissions>
  </metadata>

  <inputs>
    <input name="item_id" type="string" required="false">
      Slash-separated item path. Triggers ID mode.
    </input>
    <input name="item_type" type="string" required="false">
      Restrict to item type (ID mode only). Auto-detects if omitted.
    </input>
    <input name="query" type="string" required="false">
      Keyword search query. Triggers query mode.
    </input>
    <input name="scope" type="string" required="false">
      Item type and namespace filter for query mode.
    </input>
    <input name="source" type="string" required="false">
      Restrict resolution source (project, user, system, registry, local, all).
    </input>
    <input name="destination" type="string" required="false">
      Copy item to this space after resolving (ID mode only): project or user.
    </input>
    <input name="limit" type="integer" required="false">
      Max results for query mode (default: 10).
    </input>
  </inputs>

  <outputs>
    <output name="result">Resolved item content and metadata (ID mode) or matching items (query mode)</output>
  </outputs>
</directive>
```

<process>
  <step name="detect_mode">
    If {input:item_id} is provided, use ID mode.
    If {input:query} is provided, use query mode.
    If both are provided, return an error.
  </step>

  <step name="resolve">
    ID mode: `rye_fetch(item_id="{input:item_id}", item_type="{input:item_type}", source="{input:source}", destination="{input:destination}")`
    Query mode: `rye_fetch(query="{input:query}", scope="{input:scope}", source="{input:source}", limit={input:limit})`
  </step>

  <step name="return_result">
    Return the resolved item(s) to the caller.
  </step>
</process>
