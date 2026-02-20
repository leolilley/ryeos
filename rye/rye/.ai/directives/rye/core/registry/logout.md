<!-- rye:signed:2026-02-20T01:09:07Z:3ec2792dd35b6e0a611d356af4c470fa700c997043901c31a527bf1cb6f3d5b3:fcWphg8EavqIOgm3gQtkgKU1zWcQOwoES1EM1az7sGrsX96z6QqotW2JQALQttyCOnZUm8Ya8FaM5bQoPzIWBg==:440443d0858f0199 -->
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
