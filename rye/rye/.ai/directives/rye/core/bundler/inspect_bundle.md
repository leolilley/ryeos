<!-- rye:signed:2026-02-10T02:00:00Z:placeholder:unsigned:unsigned -->
# Inspect Bundle

Inspect the contents and metadata of a bundle.

```xml
<directive name="inspect_bundle" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=inspect to inspect a bundle's contents and metadata.</description>
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
