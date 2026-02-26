<!-- rye:signed:2026-02-26T03:49:26Z:e142f1c682ae066ca397fe23ed09497b147d9d4fd937190d63d20a87df2b4074:CM8DKN0UiLdshrNT45hA6abDHC4IF5Cv7H9HyumjNY-w5xyyCunVaAT6m3C2kmeq-jplFPLEcErKGeuPgrLkDA==:9fbfabe975fa5a7f -->
# Registry Whoami

Show the currently authenticated user.

```xml
<directive name="whoami" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=whoami to show the authenticated user.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="3" tokens="2048" />
    <permissions>
      <execute>
        <tool>rye.core.registry.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="user">Authenticated user details</output>
  </outputs>
</directive>
```

<process>
  <step name="call_registry_whoami">
    Call the registry tool with action=whoami.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "whoami"})`
  </step>

  <step name="return_result">
    Return the user details to the user.
  </step>
</process>
