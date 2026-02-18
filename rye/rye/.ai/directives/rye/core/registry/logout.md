<!-- rye:signed:2026-02-18T05:40:31Z:a2ebf7b22417be3eede7b7247626b7a82b80139b0a9f4fddeeffc5169c349cd6:tb7knkqhjAy8F03AagvumanQ6aPfSNhDI7U8ZHlQkzEqMLaUb9ScIWt68b0IZCLMl1JbGzEnaWqz4BMs6IuPDw==:440443d0858f0199 -->
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
