<!-- rye:signed:2026-02-26T05:52:23Z:50b551e444740d5fe196392d85178f4725ce69e1eed418f267a3ab901380b480:rbJRxMt00axdscGIXyVsLj_jNt5g2FEYQ_B251iPurtPzc4MwARwIvlmy-TXmyYs85XS0cChaDrllDr6qkneCA==:4b987fd4e40303ac -->
# Verify Bundle

Verify the integrity of an existing bundle.

```xml
<directive name="verify_bundle" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=verify to verify a bundle's integrity.</description>
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
