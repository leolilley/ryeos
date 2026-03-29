<!-- rye:signed:2026-03-29T06:38:41Z:afda702a3ebc2a3638cfdcfece31b92df42525543fde123768282087f023e909:cutnxK-CGZiYJqP8Wen6VGl2y8hoi4008W7tkZBSyxI5t_QZ2BSzJDyhPwLrNq4NxI4dBe4pyilyDMkU8YrBAw==:4b987fd4e40303ac -->
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
