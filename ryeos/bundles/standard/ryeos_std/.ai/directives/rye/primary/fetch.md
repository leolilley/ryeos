<!-- rye:signed:2026-04-19T09:49:53Z:553dac861ecccaf65e407fb71517714f149caae19b805256466f56d968346365:TCfePQJAkjaopsHRj/e75Ir4b+J9pScP9efR+nj7xQ+UEToYrU/rJYA1bUK4DQMoc0j0uenuU9d0iR+LRgkqAA==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
