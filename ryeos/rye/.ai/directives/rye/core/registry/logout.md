<!-- rye:signed:2026-02-21T05:56:40Z:3ec2792dd35b6e0a611d356af4c470fa700c997043901c31a527bf1cb6f3d5b3:87ECwQ-1sXClHv868rrHOjqal6tpB4MdO5HpcmLo-brtwxtoQK6L-cvGKXqR0kWUPl8HvpOhaEbmQfo0Q7sbCg==:9fbfabe975fa5a7f -->
# Registry Logout

Clear the local authentication session.

```xml
<directive name="logout" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=logout to clear the local auth session.</description>
    <category>rye/core/registry</category>
    <author>rye-os</author>
    <model tier="fast" />
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
