<!-- rye:signed:2026-02-20T01:09:07Z:eff7534269168281e215f0ff925a7f30ec14740b96cd0d0625f1093e8fd57527:gXD0P1VkibOzXriSDMiXpRIVsukqa_c96l0xwq60ZVoteYR7UDBWcKGkhn3l1A-0k6jR--6aMqumTcGZaz6aDw==:440443d0858f0199 -->
# Create Bundle

Create a new bundle using the bundler tool.

```xml
<directive name="create_bundle" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=create to create a new bundle.</description>
    <category>rye/core/bundler</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits max_turns="4" max_tokens="4096" />
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
    `rye_execute(item_type="tool", item_id="rye/core/bundler/bundler", parameters={"action": "create", "bundle_id": "{input:bundle_id}", "version": "{input:version}"})`
  </step>

  <step name="return_result">
    Return the created bundle details to the user.
  </step>
</process>
