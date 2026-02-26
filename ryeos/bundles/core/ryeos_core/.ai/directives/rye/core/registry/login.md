<!-- rye:signed:2026-02-26T05:02:34Z:313ba38e0aae8691be261f2c2cdf66908d84feb315c39b7e6cf51c5094dbb3f7:CYrDjPSMjxWwrAtHfzqDn79X_E-bke37u9PFb29c4slAiTtsg-q7B3huZ1YW1O__4BSbtJXrj6s2qnfGJv-NAQ==:4b987fd4e40303ac -->
# Registry Login

Start the device authentication flow for the registry.

```xml
<directive name="login" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=login to start a device auth flow.</description>
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
    <output name="auth_flow">Device auth flow details including verification URL and device code</output>
  </outputs>
</directive>
```

<process>
  <step name="call_registry_login">
    Call the registry tool with action=login.
    `rye_execute(item_type="tool", item_id="rye/core/registry/registry", parameters={"action": "login"})`
  </step>

  <step name="return_result">
    Return the device auth flow details to the user.
  </step>
</process>
