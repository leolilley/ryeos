<!-- rye:signed:2026-02-18T05:40:31Z:ed8e45671b1b4f2534e08a19c3dec5ae0450da86c8bcae229ef275bd915fca48:cBcyj3zvkqPJRKEXxfsOE88DpgAvf6ZKwy0gmNzlkraFZK-pPU3uoSNgR7ugAy6aC9nbBRGzeqbI_0DEbLODCw==:440443d0858f0199 -->
# Registry Whoami

Show the currently authenticated user.

```xml
<directive name="whoami" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=whoami to show the authenticated user.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="haiku" />
    <limits max_turns="3" max_tokens="2048" />
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
