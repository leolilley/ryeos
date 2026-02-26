<!-- rye:signed:2026-02-25T07:50:41Z:a5d7f943069b72d80a170628568ebe9d6bdb980a170096ce0a8360169c80c29f:AEHkwP_GdRuzxwAAFXAhlNns9tEtrFDtrRUxQ4Y5bqHkKo8Dxe-CElLx_mcIi_Kay9mrmoitmPnaeD3MslVOCQ==:9fbfabe975fa5a7f -->
<!-- rye:signed:2026-02-22T02:31:19Z:59c0aa87ec3b54a2be34f80e3e6845d64e800e8ef267be818d68d7e00e06fc54:oxBGy_OHfQQAnNX3IxFqbtRksDo-yhcc5iRfsXg7xgcs0tJ8X0xWAsJYrHaV1YJdjzHFixlSilhbf0PEsjIOBA==:9fbfabe975fa5a7f -->
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
