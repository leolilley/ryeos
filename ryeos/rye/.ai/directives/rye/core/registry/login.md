<!-- rye:signed:2026-02-21T05:56:40Z:d06db7b2c39769c510bf9375b5e0b872042f93e124944d8dac249627d49ba651:Uk7s_NYTdIccEFBm6BtSNN9DKGDC4bm2uJv2K7RaPA1udvUmz3tnbTwxNHjMpkGwwVr38AvSo70E_9dZEWdnAQ==:9fbfabe975fa5a7f -->
# Registry Login

Start the device authentication flow for the registry.

```xml
<directive name="login" version="1.0.0">
  <metadata>
    <description>Wraps the registry tool action=login to start a device auth flow.</description>
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
