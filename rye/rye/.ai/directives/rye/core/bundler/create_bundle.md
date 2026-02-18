<!-- rye:signed:2026-02-18T05:40:31Z:6fd9872127773c2dab2404808bcb7cff6a4b1d7d8bc0b2c2b7382640ed18a98f:JCFR2xKoPdSgRSSXlacdAu0mHbj74faqjEtpfkzdJkdYn8VN42kmFKixutrrQ68weN6oixQE4uzwsTcAUITBBw==:440443d0858f0199 -->
# Create Bundle

Create a new bundle using the bundler tool.

```xml
<directive name="create_bundle" version="1.0.0">
  <metadata>
    <description>Wraps bundler.py action=create to create a new bundle.</description>
    <category>rye/core/bundler</category>
    <author>rye-os</author>
    <model tier="haiku" />
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
