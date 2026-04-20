<!-- rye:signed:2026-04-19T09:49:53Z:dd4cc5763d08424f371bfe3e20409fcfa5ab982b412912f2c3691e0c90f570be:Y2C/h8BpbPvCfty2Tep2XDt8dB2XLHYvBRKXMUGjsBM2ff5UMSqO865EH6/Li0BdvP5nbgUxJODMBV1DU1ULAg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
# Initialize Remote

First-time remote setup: verify connectivity, pin remote key, sync initial state.

```xml
<directive name="init" version="1.0.0">
  <metadata>
    <description>First-time remote CAS setup and key pinning.</description>
    <category>rye/core/remote</category>
    <author>rye-os</author>
    <model tier="fast" />
    <limits turns="5" tokens="4096" />
    <permissions>
      <execute>
        <tool>rye.core.remote.*</tool>
      </execute>
    </permissions>
  </metadata>

  <inputs />

  <outputs>
    <output name="setup_result">Initialization results</output>
  </outputs>
</directive>
```

<process>
  <step name="check_api_key">
    Verify RYE_REMOTE_API_KEY is set. If not, instruct the user to set it.
  </step>

  <step name="status">
    Run remote status to verify connectivity and show current state:
    ```
    rye execute tool rye/core/remote/remote with {"action": "status"}
    ```
  </step>

  <step name="push">
    Push initial state to remote:
    ```
    rye execute tool rye/core/remote/remote with {"action": "push"}
    ```
  </step>

  <step name="report">
    Report: remote URL, objects synced, manifest hashes.
    Remind user to set secrets via the secrets directive if needed.
  </step>
</process>
