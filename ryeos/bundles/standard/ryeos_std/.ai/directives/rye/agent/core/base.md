<!-- rye:signed:2026-04-10T00:57:19Z:136701191454f742e16909f4b56f1c3bb69c8c4cc6feb1a01ce0a7358e563691:m2LaulXdfcYhuXWHiuV7pwjhdf3ihmkysf1v4wQlNemFEPo9uJ2FECCCX9Oadsb8a6vKxxopytfTWlLfC3CYCQ:4b987fd4e40303ac -->
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
