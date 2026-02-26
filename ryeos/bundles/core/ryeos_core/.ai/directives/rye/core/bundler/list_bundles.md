<!-- rye:signed:2026-02-26T05:02:34Z:707be2130959ea4ed1c101a8c8b7abaf1a7b9460521ba88bd51d2dc2df207ba5:QzvR-w9KjKC41Vu_fDJM_vLW7qXF0yHo3KFjyja3-q9nQN-nMMLgVlQ-_SN-mTdyWd1DmLtQSecFwH_Fgf3YDg==:4b987fd4e40303ac -->
# List Bundles

List all available bundles.

```xml
<directive name="list_bundles" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=list to list all available bundles.</description>
    <category>rye/core/bundler</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
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
