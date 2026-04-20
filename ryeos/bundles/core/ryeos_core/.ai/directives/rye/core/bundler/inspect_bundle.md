<!-- rye:signed:2026-04-19T09:49:53Z:3003b015ab51dfd60a527600f200fe8bb68f74881c3722386bb49e33625b4de0:cpYtH73xxA6aJqzMATFRqlH0ozD3RSx1FPHpDJANOkHqNSrWOLHsJw4gFE8lQOk+mezI7CnonSdL9Y4OsE32Cw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
    `rye_execute(item_id="rye/core/bundler/bundler", parameters={"action": "inspect", "bundle_id": "{input:bundle_id}"})`
  </step>

  <step name="return_result">
    Return the bundle information to the user.
  </step>
</process>
