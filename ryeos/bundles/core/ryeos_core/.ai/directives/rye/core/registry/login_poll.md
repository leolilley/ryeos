<!-- rye:signed:2026-02-26T06:42:50Z:f2e9e8465c9128f0970edc8e84ec5e6220aec8a799edcef21b54f5e79142b7fc:FUaQ5VdWF0h1odG9MBdJtegzUpb4QCw0IpWw43rgJkAQPCnUlVddmzYWM4QFXikR1U5ziMuIyymK3OU-hfawDg==:4b987fd4e40303ac -->
# Registry Login Poll

Poll for device authentication completion.

```xml
<directive name="login_poll" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=login_poll to poll for auth completion.</description>
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
