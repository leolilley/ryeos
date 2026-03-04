<!-- rye:signed:2026-03-03T22:32:56Z:fe695a2468c1117ed3f7a87504796298790f86e6d38779a97f28fb9c7ba77b4d:fxVlb5e0fELPmZlT50Yu9eLfN9nKfzDtve6iK4_B-cryJvuGR569U4ee4CPLuYT3WH66L7kyiL8NdfDl5z8jAg==:4b987fd4e40303ac -->
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
