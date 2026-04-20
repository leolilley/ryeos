<!-- rye:signed:2026-04-19T09:49:53Z:dc847073c8ab725317c6271f7effb865f4a0a4124da401997a71200a35464374:X/2qMG8x1cM3n9BuZ+qNMvKLf1mlcMLQUyAlhVtx77wkvV8vRxLanIihVyscQ6O1Q+vaezrc0JVLtCG/8zg0AQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
    `rye_execute(item_id="rye/core/bundler/bundler", parameters={"action": "verify", "bundle_id": "{input:bundle_id}"})`
  </step>

  <step name="return_result">
    Return the verification result to the user.
  </step>
</process>
