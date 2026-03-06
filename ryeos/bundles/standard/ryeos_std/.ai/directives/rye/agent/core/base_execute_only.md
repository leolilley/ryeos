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
