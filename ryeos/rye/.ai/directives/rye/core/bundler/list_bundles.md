<!-- rye:signed:2026-02-22T02:31:19Z:e9c1ae0e34ba4d4fc7ed7ab8ea20651274435e92e8b45a69766cb44343db5071:JXEVRpb12KQ9PtAHpvvrdp55hOLaKKrkCvvHoTKiy7qi0JqJB-P8mn_tx0TYiUKeHAvEesr6-HPeAlGtPurgAA==:9fbfabe975fa5a7f -->
# List Bundles

List all available bundles.

```xml
<directive name="list_bundles" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=list to list all available bundles.</description>
    <category>rye/core/bundler</category>
    <author>rye-os</author>
    <model tier="fast" />
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
