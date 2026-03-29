<!-- rye:signed:2026-03-29T06:50:21Z:ce39808561bc2b8874000957e69c17ecb5792fe926840534c148424bf8c4f451:DSoTQ55rHHVfFMvSHTjMeBvDutqRIzqQctTMX68IodfFahtQifvp1YRQTJvFwk4ptQYVamkX86FEZZDRQTmOAg==:4b987fd4e40303ac -->
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
      <before>rye/agent/core/protocol/fetch</before>
      <before>rye/agent/core/protocol/sign</before>
    </context>
    <permissions />
  </metadata>
</directive>
```
