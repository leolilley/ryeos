<!-- rye:unsigned -->

# Base

Standard operating context for Rye agent threads.

```xml
<directive name="base" version="1.0.0">
  <metadata>
    <description>Base operating context — standard identity, behavior, and full tool protocol</description>
    <category>rye/agent/core</category>
    <author>rye-os</author>
    <context>
      <system>rye/agent/core/Identity</system>
      <system>rye/agent/core/Behavior</system>
      <before>rye/agent/core/protocol/execute</before>
      <before>rye/agent/core/protocol/search</before>
      <before>rye/agent/core/protocol/load</before>
      <before>rye/agent/core/protocol/sign</before>
    </context>
    <permissions>
      <execute>*</execute>
      <search>*</search>
      <load>*</load>
      <sign>*</sign>
    </permissions>
  </metadata>
</directive>
```
