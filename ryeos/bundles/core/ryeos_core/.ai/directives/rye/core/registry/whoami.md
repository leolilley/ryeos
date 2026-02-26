<!-- rye:signed:2026-02-26T05:52:23Z:e142f1c682ae066ca397fe23ed09497b147d9d4fd937190d63d20a87df2b4074:2RSIx4WTLvhB_-PO0B-BRtimn9qlPkyjauj8GcA6F8N_2XjoQuk45IQQJJr-4ehaYlP_qqLAlwShc2tIWWP_DQ==:4b987fd4e40303ac -->
# Registry Whoami

Show the currently authenticated user.

```xml
<directive name="whoami" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=whoami to show the authenticated user.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.core.registry.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="user">Authenticated user details</output>
  </outputs>
</directive>
```

<process>
  <step name="call_registry_whoami">
    Call the registry tool with action=whoami.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "whoami"})`
  </step>

  <step name="return_result">
    Return the user details to the user.
  </step>
</process>
