<!-- rye:signed:2026-03-03T22:32:56Z:d459b989f46d2b77a5c89772519100b0cff9b1c819ae58b477cdb2834c4a1e78:dQl4f7g6oqk3B5ks7qOZQMXvih-mePa0ymrR_EvNqky_cjgpjUtBZ6n6yNKpOKYZG3F8dnNQJvHbouJB0vkTCQ==:4b987fd4e40303ac -->
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
