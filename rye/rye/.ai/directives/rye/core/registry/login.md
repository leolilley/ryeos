<!-- rye:signed:2026-02-18T05:40:31Z:d008af8fd76bdfd5bd7237cac1281496e5a401655608c013ac710f4ea545e0da:3gn5PqGGV5FReAl3muf3Zb9EkH-4N8jeORZ1l7teoK5tz2NbA4VjYEYAE-Q7161j0z5VkV7BYW5sBfJEd_NJCg==:440443d0858f0199 -->
# Registry Login

Start the device authentication flow for the registry.

```xml
<directive name="login" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=login to start a device auth flow.</description>
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
