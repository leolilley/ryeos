<!-- rye:signed:2026-02-20T01:09:07Z:1cb0df8f189ab077c85d49861e5ccf32a435914f2c5e78f1c9fe2ae506c1b19b:8l5_Eq1H2uvUOnr-LYiyQwQ9kLn2Vp6Qvl6YrOPnbK-W9h-CibebW5LEkMYY8i37HAJrh2Mk96KeSRWRoFORCA==:440443d0858f0199 -->
# Registry Login Poll

Poll for device authentication completion.

```xml
<directive name="login_poll" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=login_poll to poll for auth completion.</description>
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

  <inputs>
    <input name="device_code" type="string" required="true">
      The device code returned from the login flow
    </input>
  </inputs>

  <outputs>
    <output name="auth_status">Authentication status (pending, completed, or expired)</output>
  </outputs>
</directive>
```

<process>
  <step name="validate_inputs">
    Validate that {input:device_code} is non-empty.
  </step>

  <step name="call_registry_login_poll">
    Call the registry tool with action=login_poll.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "login_poll", "device_code": "{input:device_code}"})`
  </step>

  <step name="return_result">
    Return the authentication status to the user.
  </step>
</process>
