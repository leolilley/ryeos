<!-- rye:unsigned -->

# Base

Standard operating context for Rye agent threads.

```xml
<directive name="base" version="2.0.0" extends="agent/core/base">
  <metadata>
    <description>Rye agent base — extends general agent base with Rye identity and behavior</description>
    <category>rye/agent/core</category>
    <author>rye-os</author>
    <context>
      <system>rye/agent/core/Identity</system>
      <system>rye/agent/core/Behavior</system>
      <suppress>agent/core/Behavior</suppress>
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
