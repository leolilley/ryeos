<!-- rye:signed:2026-02-20T01:09:07Z:d06db7b2c39769c510bf9375b5e0b872042f93e124944d8dac249627d49ba651:yEJHiO0EPFEtHjuRbworfhTwkeaKMQG5dM1s5mMgXT_ix7YTpnlh2p7e_-hfE_S7FZP0_BWUMLQUE5CLu8RTBQ==:440443d0858f0199 -->
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
