<!-- rye:signed:2026-02-20T01:09:07Z:59c0aa87ec3b54a2be34f80e3e6845d64e800e8ef267be818d68d7e00e06fc54:VtEhwev8lyKKMh3FzKsxApM3ssnBOhIBzz_A4lvtlyzpdxJFdzc0MGO0yKsXDoxQ6UvnuB2yB9PpurROLOPKAg==:440443d0858f0199 -->
# Inspect Bundle

Inspect the contents and metadata of a bundle.

```xml
<directive name="inspect_bundle" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=inspect to inspect a bundle's contents and metadata.</description>
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
