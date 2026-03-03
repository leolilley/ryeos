<!-- rye:unsigned -->

# Base Execute Only

Narrow operating context for threads that only need to execute tools.

```xml
<directive name="base_execute_only" version="1.0.0">
  <metadata>
    <description>Narrow operating context — execute only, no search/load/sign protocol</description>
    <category>rye/agent/core</category>
    <author>rye-os</author>
    <context>
      <system>rye/agent/core/Identity</system>
      <system>rye/agent/core/Behavior</system>
      <before>rye/agent/core/protocol/execute</before>
    </context>
    <permissions>
      <execute>*</execute>
    </permissions>
  </metadata>
</directive>
```
