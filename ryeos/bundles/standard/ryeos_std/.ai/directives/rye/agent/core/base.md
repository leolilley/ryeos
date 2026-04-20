<!-- rye:signed:2026-04-19T09:49:53Z:41e4b94dc1496365b74cdc1d424149705aba7351a99e0a2203e00ff6db2f2726:w8ALyV0EeezMS1ltuXfURZolhneAL5ghcWS0i5ijE9DpKkgEzXeS1jOsA9fsbur5ovKmRgVygL65gKPDMzWhBg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
      <fetch>*</fetch>
      <sign>*</sign>
    </permissions>
  </metadata>
</directive>
```
