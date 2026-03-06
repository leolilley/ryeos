<!-- rye:signed:2026-03-05T23:57:36Z:68a7411502603baec4688d1b007d9b16515040ae5cb371e7335b8d54a9a2b5ba:j01RAVgIREyubAs0dRZz2KBYu8_zWSDXDfhzkv59VTCVCblzcwn3ydr5mdSsC8P_ImH87N1kJkRoVYJyr8IlCQ==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```xml
<directive name="base" version="1.0.0">
  <metadata>
    <description>General agent base — behavior and protocol context, no identity (agents provide their own)</description>
    <category>agent/core</category>
    <author>rye-os</author>
    <context>
      <system>agent/core/Behavior</system>
      <before>rye/agent/core/protocol/execute</before>
      <before>rye/agent/core/protocol/search</before>
      <before>rye/agent/core/protocol/load</before>
      <before>rye/agent/core/protocol/sign</before>
    </context>
    <permissions />
  </metadata>
</directive>
```
