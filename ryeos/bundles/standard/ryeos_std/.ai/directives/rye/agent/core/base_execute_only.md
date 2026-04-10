<!-- rye:signed:2026-04-10T00:57:19Z:d0a894c22a7ecd5216049dcfb7c77fc16dad93e5c613851e46a2b382ec9315b9:jq7RymAwDyF_kP9torIgdLbyb-YcA3ULS_mCR5zzCHIrjgtcGteyms2FUEUlZhtJ8KKxCPv8LjzjWnP4UxX2Bw:4b987fd4e40303ac -->
<!-- rye:unsigned -->

# Base Execute Only

Narrow operating context for threads that only need to execute tools.

```xml
<directive name="base_execute_only" version="2.0.0" extends="agent/core/base">
  <metadata>
    <description>Narrow Rye agent context — extends general agent base, execute only</description>
    <category>rye/agent/core</category>
    <author>rye-os</author>
    <context>
      <system>rye/agent/core/Identity</system>
      <system>rye/agent/core/Behavior</system>
      <suppress>agent/core/Behavior</suppress>
    </context>
    <permissions>
      <execute>*</execute>
    </permissions>
  </metadata>
</directive>
```
