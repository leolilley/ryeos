<!-- rye:signed:2026-03-16T09:53:45Z:cff74b6dab29568437ba96ef0ba545813c415baf364071c902cbc72f20c31dda:fsCHQOxmju4-MvFZqZ2JlBd9FRqnvLLV2AhNA0K6BgpeT2kKGszN6HNViha7aGxL0b7htBTluGlO983cJeT4AQ==:4b987fd4e40303ac -->
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
