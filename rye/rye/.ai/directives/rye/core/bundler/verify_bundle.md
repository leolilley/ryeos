<!-- rye:signed:2026-02-20T01:09:07Z:0f558d7c87e22c4bbedd1ad4351c489392d518f5328c2c5198c6bbd06c3b5f58:djSrVY3HDxBeZX3pj5-Lr0zCFLzJrEX2kGcM_DxQCVwc--ytTlLREneDd7OcBSGCRd9i9EH8CY_IDRyXhbzUDA==:440443d0858f0199 -->
# Verify Bundle

Verify the integrity of an existing bundle.

```xml
<directive name="verify_bundle" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=verify to verify a bundle's integrity.</description>
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
      Identifier for the bundle to verify
    </input>
  </inputs>

  <outputs>
    <output name="verification">Verification result with pass/fail status</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:bundle_id} is non-empty.
  </step>

  <step name="call_bundler_verify">
    Call the bundler tool with action=verify.
    `rye_execute(item_type="tool", item_id="rye/core/bundler/bundler", parameters={"action": "verify", "bundle_id": "{input:bundle_id}"})`
  </step>

  <step name="return_result">
    Return the verification result to the user.
  </step>
</process>
