<!-- rye:signed:2026-04-19T09:49:53Z:a546820b92b9e7c7c1ea7b6b4dce241f619e8ae13ba28d37e4b75e0941956dc1:VZURxTXi/7vQft/a62CgAyWFXD1a/XkOV8L5OwBKxq20ltZnn0VwszGWo6C/qboBypd8BNuRj6JteHGn5XapCg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
    `rye_execute(item_id="rye/core/bundler/bundler", parameters={"action": "list"})`
  </step>

  <step name="return_result">
    Return the list of bundles to the user.
  </step>
</process>
