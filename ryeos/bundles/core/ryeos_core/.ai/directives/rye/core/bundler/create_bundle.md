<!-- rye:signed:2026-04-19T09:49:53Z:4df18852edcd1cbe9cb8684cb3169801afe30338e9167f7b2f9ae17c98c33d5a:lAbE3rfBqIxHtb8212Zhb3p1W43LdjogtGHN9MGuCvmUkHuE4/Mw4i4Z0m5eGkzqhlc/rkKeyxGNF3iD+6jbCg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
# Create Bundle

Create a new bundle using the bundler tool.

```xml
<directive name="create_bundle" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=create to create a new bundle.</description>
    <category>rye/core/bundler</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="4" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.core.bundler.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs>
    <input name="bundle_id" type="string" required="true">
      Identifier for the bundle to create
    </input>
    <input name="version" type="string" required="false">
      Version string for the bundle (e.g., "1.0.0")
    </input>
  </inputs>

  <outputs>
    <output name="bundle">The created bundle details</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:bundle_id} is non-empty.
  </step>

  <step name="call_bundler_create">
    Call the bundler tool with action=create.
    `rye_execute(item_id="rye/core/bundler/bundler", parameters={"action": "create", "bundle_id": "{input:bundle_id}", "version": "{input:version}"})`
  </step>

  <step name="return_result">
    Return the created bundle details to the user.
  </step>
</process>
