<!-- rye:signed:2026-02-10T02:00:00Z:placeholder:unsigned:unsigned -->
# List Bundles

List all available bundles.

```xml
<directive name="list_bundles" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=list to list all available bundles.</description>
    <category>rye/core/bundler</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.core.bundler.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="bundles">List of available bundles</output>
  </outputs>
</directive>
```

<process>
  <step name="call_bundler_list">
    Call the bundler tool with action=list.
    `rye_execute(item_type="tool", item_id="rye/core/bundler/bundler", parameters={"action": "list"})`
  </step>

  <step name="return_result">
    Return the list of bundles to the user.
  </step>
</process>
