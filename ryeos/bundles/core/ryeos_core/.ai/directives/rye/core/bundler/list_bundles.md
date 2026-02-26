<!-- rye:signed:2026-02-26T03:49:26Z:707be2130959ea4ed1c101a8c8b7abaf1a7b9460521ba88bd51d2dc2df207ba5:oXfdc4KNhsmgiUVJ-N4Ou5ubWaOddVQwVhkUHI5hDErKH83rZGAZtRDHUEOPS_lQevhrqru1PIoFw64sMFMwBQ==:9fbfabe975fa5a7f -->
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
