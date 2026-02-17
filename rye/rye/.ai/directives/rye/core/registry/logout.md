<!-- rye:signed:2026-02-10T02:00:00Z:placeholder:unsigned:unsigned -->
# Registry Logout

Clear the local authentication session.

```xml
<directive name="logout" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=logout to clear the local auth session.</description>
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
    <output name="status">Logout confirmation</output>
  </outputs>
</directive>
```

<process>
  <step name="call_registry_logout">
    Call the registry tool with action=logout.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "logout"})`
  </step>

  <step name="return_result">
    Return the logout confirmation to the user.
  </step>
</process>
