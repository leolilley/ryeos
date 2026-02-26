<!-- rye:signed:2026-02-26T03:49:26Z:283e7ec1c3d54be7d35c1feddf752b6044991e61226e904196c4bbc7bc509236:zFyTigDpKXuRJfli0HELORJ1FsgNhK58xjxjut0BckKEeQsVNgg8tX_rZYXTAykqrPc2d5sUTOYVDLuS5nxsBw==:9fbfabe975fa5a7f -->
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
    `rye_execute(item_type="tool", item_id="rye/core/bundler/bundler", parameters={"action": "create", "bundle_id": "{input:bundle_id}", "version": "{input:version}"})`
  </step>

  <step name="return_result">
    Return the created bundle details to the user.
  </step>
</process>
