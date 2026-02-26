<!-- rye:signed:2026-02-26T05:02:34Z:a5d7f943069b72d80a170628568ebe9d6bdb980a170096ce0a8360169c80c29f:dvBrZfg34uAYMLoU0ghH23NfkTKs565ZeaLhLY8QzhaQQBbYHY-yQ02m3luVBAWC1_nmAaEvHWJl4qvufE6cAw==:4b987fd4e40303ac -->
# Inspect Bundle

Inspect the contents and metadata of a bundle.

```xml
<directive name="inspect_bundle" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=inspect to inspect a bundle's contents and metadata.</description>
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

  <inputs>
    <input name="bundle_id" type="string" required="true">
      Identifier for the bundle to inspect
    </input>
  </inputs>

  <outputs>
    <output name="bundle_info">Bundle contents and metadata</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:bundle_id} is non-empty.
  </step>

  <step name="call_bundler_inspect">
    Call the bundler tool with action=inspect.
    `rye_execute(item_type="tool", item_id="rye/core/bundler/bundler", parameters={"action": "inspect", "bundle_id": "{input:bundle_id}"})`
  </step>

  <step name="return_result">
    Return the bundle information to the user.
  </step>
</process>
